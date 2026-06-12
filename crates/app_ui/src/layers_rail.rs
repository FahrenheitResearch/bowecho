//! The LAYER RAIL — one uniform list of everything drawn over the map
//! (docs/ui-overhaul-spec.md §2, direction A "everything is a layer").
//!
//! Extracted verbatim from `radar_controls_panel`'s Layers fold (spec PR-3:
//! extraction, no movement). Every row renders through `layer_row` (row
//! grammar v2 in main.rs): [vis] [name] [state] [opacity] [up/down] [extras]
//! [gear] [remove]. The gear contract is the extensibility rule: it opens
//! the layer's owning window/tab, or a small popover — a row carries at
//! most two inline extras besides the gear and remove slots, which is what
//! keeps new features from re-crowding the rail.

use std::time::Instant;

use eframe::egui;

use crate::{
    LayerRowGear, LayerRowOpacity, LayerRowOrder, LayerRowRemove, LayerRowSpec, LayerRowVis,
    PlacefileSlot, PollSource, RadarSite, SidebarTab, ViewerApp, dock, format_site_label,
    intl_provider_label, layer_row, mesoanalysis, oa_derived,
};

impl ViewerApp {
    /// Honest layer count for the rail header (ui-refresh proposal §1.3.3):
    /// everything the rail shows as an enabled row.
    pub(crate) fn rail_layer_count(&self) -> usize {
        usize::from(self.volume.is_some())
            + self.radar_layers.len()
            + usize::from(self.sat_layer.is_some())
            + self.model_layers.len()
            + usize::from(self.obs_enabled)
            + usize::from(self.glm_enabled)
            + usize::from(!self.spc_outlooks_enabled.is_empty())
            + usize::from(self.spc_reports_enabled)
            + usize::from(self.hazards_visible && self.hazard_overlay.is_some())
            + usize::from(self.tor_tracks.show_tracks)
            + usize::from(self.tor_tracks.show_tds)
            + usize::from(self.wofs.drape_on_map)
            + usize::from(self.farm.drape.enabled)
            + self.placefile_slots.iter().filter(|s| s.enabled).count()
    }

    /// Weak uppercase group mini-header with a right-aligned action slot.
    /// Groups are deliberately NOT collapsing — the rail stays one
    /// scannable list; collapsing returns the junk-drawer dynamics
    /// (spec §2.2).
    fn rail_group_header(ui: &mut egui::Ui, label: &str, right: impl FnOnce(&mut egui::Ui)) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(label)
                    .small()
                    .strong()
                    .color(crate::SUBHEAD_COLOR),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
        });
    }

    /// The rail rows, grouped BASE → ATMOSPHERE → OBS → SEVERE → COMMUNITY
    /// (spec §2.2): primary + overlay radars + rotation tracks/TDS, then
    /// model/OA fields + GOES + drapes, then obs + lightning, then SPC +
    /// warnings, then placefiles.
    pub(crate) fn layers_rail(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Groups are weak mini-headers, NOT collapsing — the rail stays one
        // scannable list (spec §2.2). Map paint order is unchanged (the
        // compositor is layer-type-major); the grouping is a reading aid.
        let overlay_count = self.radar_layers.len();
        let mut clear_overlays = false;
        Self::rail_group_header(ui, "BASE", |ui| {
            if overlay_count > 0 {
                if crate::fixed_action_button(ui, "Clear", 52.0)
                    .on_hover_text("Remove every overlay radar")
                    .clicked()
                {
                    clear_overlays = true;
                }
                ui.weak(format!(
                    "{overlay_count} overlay{}",
                    if overlay_count == 1 { "" } else { "s" }
                ));
            }
        });
        // PRIMARY RADAR as a layer row (proposal §3-A): the old bare
        // "Radar" opacity slider, wearing the same row grammar as
        // everything else. No vis toggle / no ✕ — the primary IS the
        // app (badge ◉ instead); site/products live in the sections
        // above.
        let primary_name = match &self.volume {
            Some(volume) => {
                format!("{} {}", volume.site.id, self.selected_product.label())
            }
            None => "Radar".to_owned(),
        };
        let primary_state = if self.load_receiver.is_some() {
            "loading"
        } else if self.volume.is_none() {
            "idle"
        } else if self.realtime_level2_auto_refresh {
            "live"
        } else {
            "loaded"
        };
        if layer_row(
            ui,
            LayerRowSpec {
                vis: LayerRowVis::Badge {
                    glyph: "◉",
                    hover: "Primary radar — always drawn; site and products in the sections above",
                },
                name: &primary_name,
                name_width: crate::NAME_W_STD,
                name_hover: "Primary radar (site/products in SITE and PRODUCTS above)",
                state: Some(primary_state),
                opacity: Some(LayerRowOpacity::F32 {
                    value: &mut self.radar_opacity,
                    min: 0.15,
                    hover: "Primary radar opacity (model layer shows through)",
                }),
                ..Default::default()
            },
            |_ui| {},
        ) {
            ctx.request_repaint();
        }
        self.radar_layers_panel(ui, ctx);
        if clear_overlays {
            self.radar_layers.clear();
            self.status = "Cleared radar overlays".to_owned();
            ctx.request_repaint();
        }
        // Radar-derived algorithm layers (rotation tracks + TDS) ride with
        // the radars they derive from.
        self.tor_tracks_rail_rows(ui, ctx);
        let has_model_rows = !self.model_layers.is_empty();
        let mut step_hour: i64 = 0;
        // The Hour stepper rides the group header: it steps every
        // dock-following model row at once (spec §2.3).
        Self::rail_group_header(ui, "ATMOSPHERE", |ui| {
            if has_model_rows {
                if ui
                    .small_button("▶")
                    .on_hover_text("Next forecast hour")
                    .clicked()
                {
                    step_hour = 1;
                }
                if ui
                    .small_button("◀")
                    .on_hover_text(
                        "Previous forecast hour (layers showing the dock's variable follow)",
                    )
                    .clicked()
                {
                    step_hour = -1;
                }
                ui.weak("Hour");
            }
        });
        let mut remove_layer: Option<u64> = None;
        let mut move_layer: Option<(u64, i64)> = None;
        let mut open_model_window = false;
        // Freshness rides in the row hover now (proposal step 4) —
        // the fold's standalone freshness/ingest row is gone; deep
        // acquisition lives in the Model window's Download section.
        let newest_run_text = self
            .model_dock
            .as_ref()
            .and_then(|dock| dock.newest_run())
            .map(|(model, run, hours)| format!("{model} {run} · {hours} hrs in store"))
            .unwrap_or_else(|| "no runs in store".to_owned());
        let model_row_count = self.model_layers.len();
        for slot in &mut self.model_layers {
            let id = slot.id;
            let layer = &mut slot.layer;
            let name = format!("{} f{:02}", layer.field.key.var, layer.field.key.hour.hour);
            let name_hover = format!(
                "{} ({}) — layers draw bottom-to-top in list order\nNewest: {}",
                layer.field.key.var, layer.field.units, newest_run_text
            );
            let mut order_delta: i8 = 0;
            let mut open_window = false;
            let mut remove_this = false;
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut layer.visible,
                        hover: "Show on map (unchecked: hidden but still feeds the inspector + Alt+click soundings)",
                    },
                    name: &name,
                    // Model field keys run long (temperature_2m, …) — the
                    // standard tier truncated them to "temperature_…"
                    // (field report).
                    name_width: crate::NAME_W_WIDE,
                    name_hover: &name_hover,
                    opacity: Some(LayerRowOpacity::F32 {
                        value: &mut layer.opacity,
                        min: 0.1,
                        hover: "Model layer opacity",
                    }),
                    order: (model_row_count > 1).then_some(LayerRowOrder {
                        delta: &mut order_delta,
                    }),
                    gear: Some(LayerRowGear::Open {
                        hover: "Open the Model data window (runs · fields · soundings · download)",
                        clicked: &mut open_window,
                    }),
                    remove: Some(LayerRowRemove {
                        hover: "Remove this layer",
                        clicked: &mut remove_this,
                    }),
                    ..Default::default()
                },
                |_ui| {},
            ) {
                ctx.request_repaint();
            }
            if order_delta != 0 {
                move_layer = Some((id, order_delta as i64));
            }
            if open_window {
                open_model_window = true;
            }
            if remove_this {
                remove_layer = Some(id);
            }
        }
        if open_model_window {
            self.open_viewer(dock::WorkspacePane::Model);
        }
        if let Some(id) = remove_layer {
            self.model_layers.retain(|slot| slot.id != id);
            ctx.request_repaint();
        }
        if let Some((id, delta)) = move_layer
            && let Some(index) = self.model_layers.iter().position(|slot| slot.id == id)
        {
            let target = index as i64 + delta;
            if target >= 0 && (target as usize) < self.model_layers.len() {
                self.model_layers.swap(index, target as usize);
                ctx.request_repaint();
            }
        }
        if step_hour != 0
            && let Some(dock) = &mut self.model_dock
        {
            dock.step_hour(step_hour);
        }
        // (The model master switch + "Keep runs" retention policy
        // moved to Settings ▸ Model — proposal step 4: the fold holds
        // layers, not app policy.)
        let mut remove_sat_layer = false;
        let mut open_sat_window = false;
        if let Some(layer) = &mut self.sat_layer
            && layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut layer.visible,
                        hover: "Show GOES on map",
                    },
                    name: "GOES",
                    name_width: crate::NAME_W_STD,
                    name_hover: "GOES satellite frame (configure in the Sat window)",
                    opacity: Some(LayerRowOpacity::F32 {
                        value: &mut layer.opacity,
                        min: 0.1,
                        hover: "Satellite layer opacity",
                    }),
                    gear: Some(LayerRowGear::Open {
                        hover: "Open the Satellite window (band · sector · cadence · playback)",
                        clicked: &mut open_sat_window,
                    }),
                    remove: Some(LayerRowRemove {
                        hover: "Remove satellite layer",
                        clicked: &mut remove_sat_layer,
                    }),
                    ..Default::default()
                },
                |_ui| {},
            )
        {
            ctx.request_repaint();
        }
        if open_sat_window {
            self.open_viewer(dock::WorkspacePane::Satellite);
        }
        if remove_sat_layer {
            self.sat_layer = None;
            self.sat_layer_texture = None;
            ctx.request_repaint();
        }
        // WoFS DRAPE — the row is born from the WoFS window's "Show on map"
        // (the Sat/Model convention) and lives here like every other layer
        // (spec §2.3). The drape only draws while the window is open (its
        // pump + radar-time sync run there).
        if self.wofs.drape_on_map {
            let minute = self.wofs.minute;
            let init = self.wofs.init.clone();
            let name_hover = format!(
                "WoFS ensemble drape: {} · init {}z · f+{}m{}\nGeoreferenced onto the radar map; product/run/minute live in the WoFS window.{}",
                self.wofs.product,
                if init.is_empty() { "??" } else { &init },
                minute,
                if self.wofs.sync_to_radar {
                    " · synced to radar time"
                } else {
                    ""
                },
                if self.wofs.open {
                    ""
                } else {
                    "\nWindow closed — the drape is paused until it reopens."
                },
            );
            let mut open_wofs = false;
            let mut remove_drape = false;
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.wofs.drape_on_map,
                        hover: "Drape the current WoFS product onto the radar map",
                    },
                    name: "WoFS drape",
                    name_width: crate::NAME_W_STD,
                    name_hover: &name_hover,
                    state: Some(if self.wofs.open { "live" } else { "paused" }),
                    opacity: Some(LayerRowOpacity::F32 {
                        value: &mut self.wofs.drape_opacity,
                        min: 0.05,
                        hover: "WoFS drape opacity",
                    }),
                    gear: Some(LayerRowGear::Open {
                        hover: "Open the WoFS window (run · product · minute · soundings)",
                        clicked: &mut open_wofs,
                    }),
                    remove: Some(LayerRowRemove {
                        hover: "Remove the WoFS drape",
                        clicked: &mut remove_drape,
                    }),
                    ..Default::default()
                },
                |ui| {
                    if !init.is_empty() {
                        ui.weak(format!("{init}z+{minute}m"));
                    }
                },
            ) {
                ctx.request_repaint();
            }
            if open_wofs {
                self.open_viewer(dock::WorkspacePane::Wofs);
            }
            if remove_drape {
                self.wofs.drape_on_map = false;
                ctx.request_repaint();
            }
        }
        // FARM DRAPE — same convention: born from the FARM window's
        // "Show on map", removable here.
        if self.farm.drape.enabled {
            let live = self.farm.live_sensor().map(|s| s.name.clone());
            let mut open_farm = false;
            let mut remove_drape = false;
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.farm.drape.enabled,
                        hover: "Drape the FARM quicklook PPI onto the radar map (georeferenced)",
                    },
                    name: "FARM drape",
                    name_width: crate::NAME_W_STD,
                    name_hover: "Georeferenced FARM (DOW/COW) quicklook drape — sensor, placement, and echoes-only live in the FARM window",
                    state: Some(if live.is_some() { "live" } else { "loaded" }),
                    opacity: Some(LayerRowOpacity::F32 {
                        value: &mut self.farm.drape.opacity,
                        min: 0.15,
                        hover: "FARM drape opacity",
                    }),
                    gear: Some(LayerRowGear::Open {
                        hover: "Open the FARM window (sensors · products · placement)",
                        clicked: &mut open_farm,
                    }),
                    remove: Some(LayerRowRemove {
                        hover: "Remove the FARM drape",
                        clicked: &mut remove_drape,
                    }),
                    ..Default::default()
                },
                |ui| {
                    if let Some(name) = &live {
                        ui.weak(name.as_str());
                    }
                },
            ) {
                ctx.request_repaint();
            }
            if open_farm {
                self.open_viewer(dock::WorkspacePane::Farm);
            }
            if remove_drape {
                self.farm.drape.enabled = false;
                ctx.request_repaint();
            }
        }
        Self::rail_group_header(ui, "OBS", |ui| {
            let _ = ui;
        });
        {
            // Surface obs as a layer row; the network sub-toggles
            // violated the two-extra budget inline, so they live
            // behind the row's ⚙ popover now (spec §2.3).
            let obs_show_metar = &mut self.obs_show_metar;
            let obs_show_mesonet = &mut self.obs_show_mesonet;
            let obs_adjust_soundings = &mut self.obs_adjust_soundings;
            let obs_fetched_at = self.obs_fetched_at;
            let obs_station_count = self.surface_obs.station_count;
            let obs_fetching = self.obs_rx.is_some();
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.obs_enabled,
                        hover: "METAR station plots: temperature/dewpoint (units per Settings ▸ Display), wind barbs, gusts — every reporting station, refreshed ~5 min",
                    },
                    name: "Surface obs",
                    name_width: crate::NAME_W_STD,
                    name_hover: "METAR station plots: temperature/dewpoint (units per Settings ▸ Display), wind barbs, gusts — every reporting station, refreshed ~5 min",
                    gear: Some(LayerRowGear::Menu {
                        hover: "Networks: METAR · Mesonet · obs-adjusted soundings",
                        content: Box::new(|ui| {
                            ui.checkbox(obs_show_metar, "METAR")
                                .on_hover_text("Airport-grade ASOS/AWOS stations");
                            ui.checkbox(obs_show_mesonet, "Mesonet")
                                        .on_hover_text(
                                            "IEM RWIS road sensors + DCP/RAWS networks — denser but lower siting quality (road sensors read hot in sun); uncheck for strict-QC METAR-only",
                                        );
                            if ui
                                        .checkbox(obs_adjust_soundings, "Obs-adjusted soundings")
                                        .on_hover_text(
                                            "The skew-T's surface T/Td/wind come from the nearest station (within 30 km, fresher than 60 min) instead of the model — parcels recompute from the REAL surface. The title shows which station adjusted it.",
                                        )
                                        .changed()
                                    {
                                        ui.ctx().request_repaint();
                                    }
                            ui.separator();
                            ui.weak("Appearance controls land here next.");
                        }),
                    }),
                    ..Default::default()
                },
                |ui| {
                    if let Some(at) = obs_fetched_at {
                        ui.weak(format!(
                            "{} stn · {}m ago",
                            obs_station_count,
                            at.elapsed().as_secs() / 60
                        ));
                    }
                    if obs_fetching {
                        ui.spinner();
                    }
                },
            ) {
                ctx.request_repaint();
            }
        }
        // LIGHTNING (GLM) — promoted from a bare checkbox to the row
        // grammar (spec §2.3). No opacity in v1: age-fade is
        // intrinsic to the layer.
        {
            let flash_count = if self.glm_enabled {
                self.glm.as_ref().map(|glm| {
                    let frame_ms = chrono::Utc::now().timestamp_millis();
                    let window_min = self.style_registry.glm().window_minutes;
                    (glm.frame_flashes(frame_ms, window_min).count(), window_min)
                })
            } else {
                None
            };
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.glm_enabled,
                        hover: "GOES GLM flashes, free via AWS (no key): trailing 10-minute window, age-faded, time-synced to the radar loop. First data ~1 min after enabling (S3 poll + granule decode).",
                    },
                    name: "Lightning",
                    name_width: crate::NAME_W_STD,
                    name_hover: "GOES GLM lightning flashes (trailing window, age-faded, loop-synced)",
                    gear: Some(LayerRowGear::Menu {
                        hover: "Lightning layer options",
                        content: Box::new(|ui| {
                            ui.weak("Source: GOES-East GLM (fixed for now;");
                            ui.weak("satellite pick lands here next).");
                            ui.separator();
                            ui.weak("Appearance controls land here next.");
                        }),
                    }),
                    ..Default::default()
                },
                |ui| {
                    if let Some((count, window_min)) = flash_count {
                        ui.weak(format!("{count} fl/{window_min}m"));
                    }
                },
            ) {
                self.save_overlay_defaults();
                ctx.request_repaint();
            }
        }
        Self::rail_group_header(ui, "SEVERE", |ui| {
            let _ = ui;
        });
        // SPC OUTLOOK — one row (spec §2.3): vis = any kind enabled
        // (off remembers the set, on restores it); ⚙ jumps to the SEVERE
        // tab's SPC outlooks section (day + kinds live there now).
        {
            let mut spc_on = !self.spc_outlooks_enabled.is_empty();
            let name = format!("SPC D{} outlook", self.spc_day);
            let fetching = self.spc_rx.is_some();
            let mut open_severe = false;
            let vis_changed = layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut spc_on,
                        hover: "Convective outlooks in SPC's own colors, archive-aware: shows the displayed day's outlook",
                    },
                    name: &name,
                    name_width: crate::NAME_W_STD,
                    name_hover: "SPC convective outlook polygons. Off remembers your kind set; on restores it. ⚙ opens the SEVERE tab's SPC outlooks section (day + kinds).",
                    gear: Some(LayerRowGear::Open {
                        hover: "Configure in the SEVERE tab: day · categorical / tornado / wind / hail",
                        clicked: &mut open_severe,
                    }),
                    ..Default::default()
                },
                |ui| {
                    if fetching {
                        ui.spinner();
                    }
                },
            );
            if vis_changed {
                if spc_on {
                    self.spc_outlooks_enabled = if self.spc_kinds_memory.is_empty() {
                        vec!["cat".to_owned()]
                    } else {
                        self.spc_kinds_memory.clone()
                    };
                } else {
                    if !self.spc_outlooks_enabled.is_empty() {
                        self.spc_kinds_memory = self.spc_outlooks_enabled.clone();
                    }
                    self.spc_outlooks_enabled.clear();
                }
                self.spc_data.fetched_at = None;
                self.save_overlay_defaults();
                ctx.request_repaint();
            }
            if open_severe {
                self.sidebar_tab = SidebarTab::Severe;
                self.set_section_open("severe_spc_outlooks", true);
            }
        }
        // SPC REPORTS — its own row (it was a checkbox hiding inside
        // the outlook row's config line).
        {
            let mut open_severe = false;
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.spc_reports_enabled,
                        hover: "Today's filtered storm reports (tornado / wind / hail), refreshed ~5 min",
                    },
                    name: "SPC reports",
                    name_width: crate::NAME_W_STD,
                    name_hover: "SPC storm report dots for the displayed day",
                    gear: Some(LayerRowGear::Open {
                        hover: "Open the SEVERE tab",
                        clicked: &mut open_severe,
                    }),
                    ..Default::default()
                },
                |_ui| {},
            ) {
                self.spc_data.fetched_at = None;
                self.save_overlay_defaults();
                ctx.request_repaint();
            }
            if open_severe {
                self.sidebar_tab = SidebarTab::Severe;
            }
        }
        // WARNINGS — the polygon layer finally appears in the layer
        // model (spec §2.3). Opacity = the same fill alpha the
        // Warnings tab's slider edits (one state, two views).
        {
            let mut fill_alpha = self.style_registry.hazard_global().fill_alpha;
            let active_count = self
                .hazard_overlay
                .as_ref()
                .map(|overlay| overlay.records.len())
                .unwrap_or(0);
            let mut open_severe = false;
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.hazards_visible,
                        hover: "NWS warning/watch/MD polygons on the map",
                    },
                    name: "Warnings",
                    name_width: crate::NAME_W_STD,
                    name_hover: "Warning polygons (filters + full text in the warnings tab)",
                    opacity: Some(LayerRowOpacity::U8 {
                        value: &mut fill_alpha,
                        min: 0,
                        max: 80,
                    }),
                    gear: Some(LayerRowGear::Open {
                        hover: "Open the warnings tab (filters · fill · full text)",
                        clicked: &mut open_severe,
                    }),
                    ..Default::default()
                },
                |ui| {
                    if active_count > 0 {
                        ui.weak(format!("{active_count} act"));
                    }
                },
            ) {
                if fill_alpha != self.style_registry.hazard_global().fill_alpha {
                    self.style_settings.hazard_global.fill_alpha = Some(fill_alpha);
                    self.rebuild_style_registry();
                    self.save_styles();
                }
                ctx.request_repaint();
            }
            if open_severe {
                self.sidebar_tab = SidebarTab::Severe;
            }
        }
        Self::rail_group_header(ui, "COMMUNITY", |ui| {
            let _ = ui;
        });
        ui.horizontal(|ui| {
            let url_response = ui.add(
                egui::TextEdit::singleline(&mut self.placefile_url_input)
                    .hint_text("https://… placefile URL")
                    .desired_width(190.0),
            );
            if self.placefile_input_focus {
                self.placefile_input_focus = false;
                url_response.request_focus();
            }
            if ui.button("Add").clicked() {
                let url = self.placefile_url_input.trim().to_owned();
                if url.starts_with("http")
                    && !self.placefile_slots.iter().any(|slot| slot.url == url)
                {
                    let mut slot = PlacefileSlot::new(url, true);
                    slot.show_text = self.style_registry.placefiles().default_show_text;
                    self.placefile_slots.push(slot);
                    self.placefile_url_input.clear();
                    self.save_placefile_settings();
                    ctx.request_repaint();
                }
            }
        });
        let mut remove: Option<usize> = None;
        let mut changed = false;
        let mut placefiles_dirty = false;
        for (index, slot) in self.placefile_slots.iter_mut().enumerate() {
            let title = slot
                .data
                .as_ref()
                .map(|p| p.title.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| slot.url.clone());
            let hover = format!(
                "{}
{}",
                slot.url, slot.status
            );
            // Field-split the slot so the row's vis toggle and the
            // trailing refresh button can borrow disjoint fields.
            let enabled = &mut slot.enabled;
            let next_refresh = &mut slot.next_refresh;
            let show_text = &mut slot.show_text;
            let mut remove_this = false;
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: enabled,
                        hover: "Show this placefile on the map",
                    },
                    name: &title,
                    name_width: crate::NAME_W_WIDE,
                    name_hover: &hover,
                    gear: Some(LayerRowGear::Menu {
                        hover: "Placefile options",
                        content: Box::new(|ui| {
                            ui.weak("Appearance controls (icon/font/line");
                            ui.weak("scales) land here next.");
                        }),
                    }),
                    remove: Some(LayerRowRemove {
                        hover: "Remove placefile",
                        clicked: &mut remove_this,
                    }),
                    ..Default::default()
                },
                |ui| {
                    if ui
                        .selectable_label(*show_text, "T")
                        .on_hover_text("Draw the file's text labels (off = icons only)")
                        .clicked()
                    {
                        *show_text = !*show_text;
                        placefiles_dirty = true;
                    }
                    if ui.small_button("↻").on_hover_text("Refresh now").clicked() {
                        *next_refresh = Some(Instant::now());
                    }
                },
            ) {
                changed = true;
            }
            if remove_this {
                remove = Some(index);
            }
        }
        if let Some(index) = remove {
            self.placefile_slots.remove(index);
            changed = true;
        }
        if changed || placefiles_dirty {
            self.save_placefile_settings();
            ctx.request_repaint();
        }
    }

    /// ANALYSIS (OA) — compute that *emits* layers lives at the rail's
    /// bottom, not among the rows (spec §2.5): Bratseth obs-correction of
    /// the dock's current surface field, RAOB soundings, and the SPC
    /// composite suite. Default-closed; gated on a model hour existing —
    /// the disabled-state hints inside explain themselves.
    pub(crate) fn oa_analysis_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // MESOANALYSIS: Bratseth obs-correction of the dock's
        // current surface field, stacked as its own "(OA)" layer.
        // OA tools render whenever a model hour exists — gating the
        // whole section on the DISPLAYED variable made the buttons
        // vanish after restarts (field report). Disabled states
        // explain themselves instead.
        let oa_var = self
            .model_dock
            .as_ref()
            .and_then(|dock| dock.latest_field())
            .map(|field| field.key.var.clone())
            .filter(|var| mesoanalysis::config_for(var).is_some());
        let dock_has_field = self
            .model_dock
            .as_ref()
            .and_then(|dock| dock.latest_field())
            .is_some();
        if !dock_has_field {
            return;
        }
        ui.add_space(4.0);
        let oa_open = self.section_open("layers_analysis_oa", false);
        let oa_response = egui::CollapsingHeader::new("Analysis (OA)")
            .id_salt("oa_analysis_fold")
            .open(Some(oa_open))
            .show(ui, |ui| {
                    let var = oa_var.clone().unwrap_or_default();
                    ui.horizontal(|ui| {
                        let ready = oa_var.is_some()
                            && self.obs_enabled
                            && !self.surface_obs.is_empty()
                            && self.model_lut.is_some()
                            && self.oa_rx.is_none();
                        if ui
                            .add_enabled(ready, egui::Button::new("Analyze obs"))
                            .on_hover_text(format!(
                                "Bratseth objective analysis: correct {var} with the live surface obs (converges to Optimal Interpolation; Bratseth 1986, ADAS weights, RTMA-style QC). Adds a \"{var} (OA)\" layer.",
                            ))
                            .clicked()
                        {
                            self.start_mesoanalysis(ctx);
                        }
                        if self.oa_rx.is_some() {
                            ui.spinner();
                        }
                        if !self.obs_enabled {
                            ui.weak("← turn on Surface obs above");
                        } else if self.surface_obs.is_empty() {
                            ui.weak("waiting for obs fetch…");
                        } else if self.model_lut.is_none() {
                            ui.weak("← \"Show on radar map\" first (Model window)");
                        } else if oa_var.is_none() {
                            ui.weak("show T2m / Td2m / 10m wind to analyze");
                        }
                    });
                    if let Some(summary) = &self.oa_last_summary {
                        ui.weak(summary);
                    }
                    // OBSERVED sounding: nearest RAOB launch, rendered by
                    // the same native skew-T (full sharprs suite on real
                    // radiosonde data). Archive-aware: uses the displayed
                    // frame's time.
                    ui.horizontal(|ui| {
                        if ui
                            .button("Obs sounding (RAOB)")
                            .on_hover_text(
                                "Nearest radiosonde launch to the map center, at the synoptic time before the displayed frame (IEM archive, no key). Renders in the native skew-T with the full parameter suite.",
                            )
                            .clicked()
                        {
                            self.start_raob_sounding(ctx);
                        }
                    });
                    // FULL SPC-mesoanalysis composites: one heavy pass
                    // (sharprs compute_all_params per cell, OA-corrected
                    // surface incl winds) caches the 64-field catalog
                    // suite (docs/spc-catalog.md); each is then an
                    // instant layer.
                    ui.horizontal(|ui| {
                        let busy = self.oa_comp_rx.is_some();
                        let ready = self.obs_enabled
                            && !self.surface_obs.is_empty()
                            && self.model_lut.is_some()
                            && !busy;
                        if self.oa_composites.is_none() {
                            if ui
                                .add_enabled(ready, egui::Button::new("Compute composites (SCP/STP/…)"))
                                .on_hover_text(
                                    "One pass computes the full SPC suite (SCP, STP, SHIP, EHI, effective SRH/shear, K-index, PW, …) from the obs-corrected surface + model profiles — then every field is an instant layer. ~30-90 s background.",
                                )
                                .clicked()
                            {
                                self.start_oa_composites(ctx);
                            }
                        } else {
                            ui.menu_button("Composites ▾", |ui| {
                                let mut pick: Option<usize> = None;
                                let _ = &mut pick;
                                // Grouped like the SPC Mesoscale Analysis
                                // page sections (docs/spc-catalog.md).
                                if let Some(fields) = &self.oa_composites {
                                    for group in oa_derived::GROUPS {
                                        if !fields.iter().any(|f| f.group == group) {
                                            continue;
                                        }
                                        ui.menu_button(group, |ui| {
                                            for (i, field) in fields
                                                .iter()
                                                .enumerate()
                                                .filter(|(_, f)| f.group == group)
                                            {
                                                if ui.button(field.name).clicked() {
                                                    pick = Some(i);
                                                    ui.close();
                                                }
                                            }
                                        });
                                    }
                                }
                                if let Some(p) = pick {
                                    self.oa_comp_pick = Some(p);
                                }
                                ui.separator();
                                if ui.button("Recompute").clicked() {
                                    self.oa_composites = None;
                                    self.start_oa_composites(ctx);
                                    ui.close();
                                }
                            });
                        }
                        if busy {
                            ui.spinner();
                            let done = self
                                .oa_comp_progress
                                .load(std::sync::atomic::Ordering::Relaxed);
                            ui.weak(format!("{done}/{} cells", self.oa_comp_total));
                        }
                    });
                    if let Some(pick) = self.take_composite_pick() {
                        self.push_composite_layer(pick, ctx);
                    }
                    // SPC-style derived product: analyze the surface, then
                    // recompute CAPE from the corrected surface + profiles.
                    ui.horizontal(|ui| {
                        let ready = self.obs_enabled
                            && !self.surface_obs.is_empty()
                            && self.model_lut.is_some()
                            && self.oa_cape_rx.is_none();
                        ui.add_enabled_ui(ready, |ui| {
                            ui.menu_button("Derive (OA) ▾", |ui| {
                                for product in oa_derived::OaProduct::ALL {
                                    if ui.button(product.label()).clicked() {
                                        self.start_oa_derive(product, ctx);
                                        ui.close();
                                    }
                                }
                                ui.separator();
                                ui.weak("Surface-driven thermo only — obs can't\ncorrect winds aloft (SRH/shear stay model).");
                            })
                            .response
                            .on_hover_text(
                                "SPC-mesoanalysis-style derived fields: Bratseth-correct the surface with live obs, then recompute the parameter from the corrected surface + model profiles (analyze-then-derive, Bothwell et al. 2002).",
                            );
                        });
                        if self.oa_cape_rx.is_some() {
                            ui.spinner();
                        }
                    });
            });
        if oa_response.header_response.clicked() {
            self.set_section_open("layers_analysis_oa", !oa_open);
        }
    }

    /// LIVE FEEDS — GR2A-style dir.list URL polling plus the international
    /// open-data feeds. This is acquisition (it replaces the primary volume
    /// source), not a layer — it lives in the DATA tab (spec §1).
    pub(crate) fn live_feeds_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
                    ui.label("Poll URL").on_hover_text(
                        "GR2A-style polling: a served directory containing dir.list (the convention DOW/mobile radar crews use). Newest file loads automatically every 15 s, decoded natively (Level II or DORADE), and joins the frame loop.",
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.poll_url)
                            .hint_text("http://host:port/path")
                            .desired_width(150.0),
                    );
                    ui.menu_button("Feeds ▾", |ui| {
                        ui.weak("research radars serving raw Level II");
                        // Grown from the same community table the map
                        // markers draw from, grouped by state — menu and
                        // markers stay in lockstep by construction.
                        let feeds = data_source::community_feeds::community_feeds();
                        let mut states: Vec<&'static str> =
                            feeds.iter().map(|feed| feed.state).collect();
                        states.sort_unstable();
                        states.dedup();
                        egui::ScrollArea::vertical()
                            .id_salt("community_feed_menu_list")
                            .max_height(340.0)
                            .show(ui, |ui| {
                                for (index, state) in states.iter().enumerate() {
                                    if index > 0 {
                                        ui.separator();
                                    }
                                    ui.weak(*state);
                                    for feed in
                                        feeds.iter().filter(|feed| feed.state == *state)
                                    {
                                        if ui
                                            .button(format!("{} — {}", feed.id, feed.label))
                                            .clicked()
                                        {
                                            self.start_known_feed_poll(feed.poll_url);
                                            ui.close();
                                        }
                                    }
                                }
                            });
                    })
                    .response
                    .on_hover_text(
                        "Community research-radar poll roots (IEM Level II host, ND State Water Commission, self-hosted university radars) — radars that aren't NEXRAD sites. Community-contributed catalog; the same sites are click-to-poll teal markers on the map.",
                    );
                    let label = if self.poll_active { "Stop" } else { "Start" };
                    if ui.button(label).clicked() {
                        self.poll_active = !self.poll_active;
                        self.poll_last_file = None;
                        self.poll_next = None;
                        if self.poll_active {
                            self.set_custom_url_poll_source();
                            // Drop any in-flight auto-refresh load: it
                            // would land after the first poll install and
                            // wipe the polled frames.
                            self.load_receiver = None;
                        }
                        if self.poll_active && self.app_settings.poll_url != self.poll_url {
                            self.app_settings.poll_url = self.poll_url.clone();
                            let _ = self.app_settings.save();
                        }
                    }
                    if self.poll_active && matches!(self.poll_source, PollSource::CustomUrl(_)) {
                        ui.weak(
                            self.poll_last_file
                                .as_deref()
                                .unwrap_or("waiting for dir.list…"),
                        );
                    }
                });
        self.intl_feeds_row(ui, ctx);
    }

    /// Start the shared poller on a known research-feed poll root — the
    /// Feeds-menu click path, reused verbatim by the community map
    /// markers so there is exactly one custom-URL start sequence.
    pub(crate) fn start_known_feed_poll(&mut self, url: &str) {
        self.poll_url = url.to_owned();
        self.set_custom_url_poll_source();
        self.poll_active = true;
        self.poll_last_file = None;
        self.poll_next = None;
        // An auto-refresh load already in flight would land AFTER the
        // first poll install and wipe the polled frames — drop it.
        self.load_receiver = None;
        self.app_settings.poll_url = self.poll_url.clone();
        let _ = self.app_settings.save();
    }

    /// Point the shared poller at the URL text field (the custom-URL
    /// Start/known-feed click). Switching away from a different source
    /// drops a still-in-flight tick so it can't install under the new one.
    fn set_custom_url_poll_source(&mut self) {
        let source = PollSource::CustomUrl(self.poll_url.clone());
        if self.poll_source != source {
            self.poll_rx = None;
            self.poll_source = source;
        }
    }

    /// INTERNATIONAL — national open-data radar feeds from data_source's
    /// provider registry (providers from other lanes appear here
    /// automatically once registered in `intl_providers()`). Picking a
    /// site starts the shared poller in Intl mode; Start resumes the
    /// persisted last selection, mirroring the poll URL row.
    fn intl_feeds_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut list_provider: Option<String> = None;
        let mut start: Option<(String, String)> = None;
        ui.horizontal(|ui| {
            ui.label("International").on_hover_text(
                "National open-data radar networks (ODIM_H5 polar volumes — OPERA Data Information Model), decoded natively. Pick a country's provider, then a site: the newest volume polls every 60 s and joins the frame loop like any polled feed.",
            );
            let provider_button = if self.intl_picker_provider.is_empty() {
                "Country ▾".to_owned()
            } else {
                format!("{} ▾", intl_provider_label(&self.intl_picker_provider))
            };
            ui.menu_button(provider_button, |ui| {
                ui.set_min_width(190.0);
                let providers = data_source::international::intl_providers();
                // Group by country; registry order within a country.
                let mut countries: Vec<&'static str> = providers
                    .iter()
                    .map(|provider| provider.country())
                    .collect();
                countries.sort_unstable();
                countries.dedup();
                for (index, country) in countries.iter().enumerate() {
                    if index > 0 {
                        ui.separator();
                    }
                    ui.weak(*country);
                    for provider in providers
                        .iter()
                        .filter(|provider| provider.country() == *country)
                    {
                        if ui.button(provider.label()).clicked() {
                            list_provider = Some(provider.id().to_owned());
                            ui.close();
                        }
                    }
                }
            })
            .response
            .on_hover_text("National feed providers, grouped by country");
            if self.intl_sites_rx.is_some() {
                ui.spinner();
            }
            if let Some(sites) = &self.intl_sites {
                ui.menu_button("Site ▾", |ui| {
                    ui.set_min_width(160.0);
                    egui::ScrollArea::vertical()
                        .id_salt("intl_site_list")
                        .max_height(300.0)
                        .show(ui, |ui| {
                            for site in sites {
                                if ui.button(&site.label).clicked() {
                                    start = Some((
                                        site.provider_id.to_owned(),
                                        site.site_id.clone(),
                                    ));
                                    ui.close();
                                }
                            }
                        });
                });
            }
            let intl_polling =
                self.poll_active && matches!(self.poll_source, PollSource::Intl { .. });
            if intl_polling {
                if ui.button("Stop").clicked() {
                    self.poll_active = false;
                    self.poll_last_file = None;
                    self.poll_next = None;
                }
                // Same status grammar as the URL poll: the dedupe key of
                // the installed frame (Polled:/Poll: live in self.status).
                ui.weak(
                    self.poll_last_file
                        .as_deref()
                        .unwrap_or("waiting for catalog…"),
                );
            } else if let Some(PollSource::Intl {
                provider_id,
                site_id,
            }) = PollSource::intl_from_settings(&self.app_settings)
            {
                // Resume the persisted selection (mirrors poll_url Start).
                if ui
                    .button("Start")
                    .on_hover_text(format!(
                        "Resume {} {site_id}",
                        intl_provider_label(&provider_id)
                    ))
                    .clicked()
                {
                    start = Some((provider_id, site_id));
                }
            }
        });
        if let Some(provider_id) = list_provider {
            self.start_intl_site_listing(&provider_id, ctx);
        }
        if let Some((provider_id, site_id)) = start {
            self.start_intl_poll(provider_id, site_id);
        }
    }

    /// `+ Add layer ▾` — THE single front door for every map data type
    /// (spec §2.4): you never need to know that layers are born inside the
    /// Model/Sat windows' "Show on radar map" buttons.
    pub(crate) fn add_layer_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // THE single front door for every map data type (proposal
        // section 4 step 5 / discoverability fix 1.4): you no longer
        // need to know that layers are born inside the Model/Sat
        // windows' "Show on radar map" buttons.
        ui.add_space(4.0);
        let mut add_site: Option<RadarSite> = None;
        ui.menu_button("+ Add layer ▾", |ui| {
                    ui.menu_button("Radar overlay", |ui| {
                        ui.set_min_width(220.0);
                        egui::ScrollArea::vertical()
                            .id_salt("add_layer_site_list")
                            .max_height(300.0)
                            .show(ui, |ui| {
                                // Favorites first (spec §2.4), then the
                                // full alphabetical list.
                                let favorites: Vec<&RadarSite> = self
                                    .sites
                                    .iter()
                                    .filter(|site| {
                                        self.app_settings.is_favorite(&site.level2_id)
                                    })
                                    .collect();
                                if !favorites.is_empty() {
                                    for site in favorites {
                                        if ui
                                            .button(format!("★ {}", format_site_label(site)))
                                            .clicked()
                                        {
                                            add_site = Some(site.clone());
                                            ui.close();
                                        }
                                    }
                                    ui.separator();
                                }
                                for site in &self.sites {
                                    if ui.button(format_site_label(site)).clicked() {
                                        add_site = Some(site.clone());
                                        ui.close();
                                    }
                                }
                            });
                    })
                    .response
                    .on_hover_text(
                        "Another radar drawn over the map (tip: right-click the map → \"lowest beam here\" does this too)",
                    );
                    if ui
                        .button("Model field…")
                        .on_hover_text(
                            "Open the Model window: pick a run + field, then \"Show on radar map\"",
                        )
                        .clicked()
                    {
                        // Same intent rule as the top-bar Model button.
                        self.model_enabled = true;
                        self.open_viewer(dock::WorkspacePane::Model);
                        // No data yet? Land the user on the Download section.
                        if self
                            .model_dock
                            .as_ref()
                            .and_then(|dock| dock.newest_run())
                            .is_none()
                        {
                            self.model_download_open = true;
                        }
                        ui.close();
                    }
                    if ui
                        .button("SpotterNetwork (placefile)")
                        .on_hover_text(
                            "Add the public SpotterNetwork positions placefile (spotter icons, 1-min refresh)",
                        )
                        .clicked()
                    {
                        let url = "https://www.spotternetwork.org/feeds/gr.txt".to_owned();
                        if !self.placefile_slots.iter().any(|slot| slot.url == url) {
                            let mut slot = PlacefileSlot::new(url, true);
                            slot.show_text = false; // dots only; hover has the card
                            self.placefile_slots.push(slot);
                            self.save_placefile_settings();
                        }
                        ui.close();
                    }
                    if ui
                        .button("Get model data… (download)")
                        .on_hover_text(
                            "Open the Model window's Download section: Fetch latest one-click ingest, or any run/hours with size + compute estimates",
                        )
                        .clicked()
                    {
                        self.model_enabled = true;
                        self.open_viewer(dock::WorkspacePane::Model);
                        self.model_download_open = true;
                        ui.close();
                    }
                    if ui
                        .button("Satellite (GOES)…")
                        .on_hover_text(
                            "Open the Satellite window: configure the follow, then \"Show on radar map\"",
                        )
                        .clicked()
                    {
                        self.open_viewer(dock::WorkspacePane::Satellite);
                        ui.close();
                    }
                    if ui
                        .button("WoFS drape…")
                        .on_hover_text(
                            "Open the WoFS window: pick a run + product, then \"Show on map\" creates the rail row",
                        )
                        .clicked()
                    {
                        self.open_viewer(dock::WorkspacePane::Wofs);
                        ui.close();
                    }
                    if ui
                        .button("FARM drape…")
                        .on_hover_text(
                            "Open the FARM window: pick a mobile radar, then \"Show on map\" creates the rail row",
                        )
                        .clicked()
                    {
                        self.open_viewer(dock::WorkspacePane::Farm);
                        ui.close();
                    }
                    // The extensible home for the OA/composites catalog
                    // (spec §2.4): post-compute, every SPC-mesoanalysis
                    // field is an instant layer from here, grouped like
                    // SPC's mesoanalysis page.
                    ui.menu_button("Mesoanalysis (OA)", |ui| {
                        let mut pick: Option<usize> = None;
                        match &self.oa_composites {
                            Some(fields) => {
                                for group in oa_derived::GROUPS {
                                    if !fields.iter().any(|f| f.group == group) {
                                        continue;
                                    }
                                    ui.menu_button(group, |ui| {
                                        for (i, field) in fields
                                            .iter()
                                            .enumerate()
                                            .filter(|(_, f)| f.group == group)
                                        {
                                            if ui.button(field.name).clicked() {
                                                pick = Some(i);
                                                ui.close();
                                            }
                                        }
                                    });
                                }
                            }
                            None => {
                                ui.weak("No composite suite computed yet —");
                                ui.weak("run \"Compute composites\" in the");
                                ui.weak("Analysis (OA) section below.");
                            }
                        }
                        if let Some(p) = pick {
                            self.oa_comp_pick = Some(p);
                        }
                    })
                    .response
                    .on_hover_text(
                        "SPC-mesoanalysis composite fields (SCP, STP, SHIP, EHI, …) as instant layers once the suite is computed",
                    );
                    if ui
                        .button("Surface obs")
                        .on_hover_text(
                            "METAR/mesonet station plots: temperature/dewpoint, wind barbs, gusts",
                        )
                        .clicked()
                    {
                        self.obs_enabled = true;
                        ctx.request_repaint();
                        ui.close();
                    }
                    if ui
                        .button("Placefile URL…")
                        .on_hover_text("Paste a GR-style placefile URL (box above)")
                        .clicked()
                    {
                        self.placefile_input_focus = true;
                        ui.close();
                    }
                });
        if let Some(site) = add_site {
            self.add_or_refresh_radar_layer(site, ctx);
        }
        // A composite picked from the menu becomes a layer immediately, even
        // while the Analysis (OA) section is collapsed (its own consumer
        // only runs when its fold is open).
        if let Some(pick) = self.take_composite_pick() {
            self.push_composite_layer(pick, ctx);
        }
    }
}
