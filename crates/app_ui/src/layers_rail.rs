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

use std::time::{Duration, Instant};

use chrono::TimeZone;
use color_tables::ColorTableFamily;
use data_source::grid_products::imgw::IMGW_POLRAD_SITES;
use data_source::sites::SiteRef;
use eframe::egui;
use ui_core::loop_engine::FeedSource;

use crate::{
    GLM_LIVE_MAX_AGE_MINUTES, ItalyDpcMapProduct, LayerRowGear, LayerRowOpacity, LayerRowOrder,
    LayerRowRemove, LayerRowSpec, LayerRowVis, PlacefileSlot, RadarSite, SidebarTab, ViewerApp,
    aircraft_soundings, compact_layer_status, compact_layer_success_age, compact_status_age,
    custom_poll_entry_label, custom_poll_entry_lat_lon, custom_poll_links_from_gis, dock, eumetsat,
    farm_frame_time_utc, format_site_label, glm_latest_age_minutes, glm_latest_is_live,
    glm_satellite_label, grid_composites, intl_provider_label, layer_row, mesoanalysis,
    normalized_poll_url, oa_derived, panel_kit, parse_custom_poll_marker_inputs, poll_url_name,
    poll_urls_match, spc_layers, warning_live_success_age,
};

fn model_map_layer_visibility_hover(grid_composite: bool) -> &'static str {
    if grid_composite {
        "Show this gridded radar/composite layer on the map"
    } else {
        "Show on map (unchecked: hidden but still feeds the inspector + Alt+click soundings)"
    }
}

fn model_map_layer_opacity_hover(grid_composite: bool) -> &'static str {
    if grid_composite {
        "Gridded radar/composite layer opacity"
    } else {
        "Model layer opacity"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OaAnalysisReadiness {
    NeedsModelField,
    NeedsSupportedField,
    NeedsSurfaceObs,
    WaitingForSurfaceObs,
    NeedsMapLayer,
    Busy,
    Ready,
}

fn oa_analysis_readiness(
    dock_has_field: bool,
    has_supported_field: bool,
    surface_obs_enabled: bool,
    surface_obs_available: bool,
    model_map_layer_available: bool,
    busy: bool,
) -> OaAnalysisReadiness {
    if !dock_has_field {
        OaAnalysisReadiness::NeedsModelField
    } else if !has_supported_field {
        OaAnalysisReadiness::NeedsSupportedField
    } else if !surface_obs_enabled {
        OaAnalysisReadiness::NeedsSurfaceObs
    } else if !surface_obs_available {
        OaAnalysisReadiness::WaitingForSurfaceObs
    } else if !model_map_layer_available {
        OaAnalysisReadiness::NeedsMapLayer
    } else if busy {
        OaAnalysisReadiness::Busy
    } else {
        OaAnalysisReadiness::Ready
    }
}

const PLACEFILE_VISIBILITY_RANGE_PERCENTS: [u16; 5] = [100, 200, 400, 800, u16::MAX];

#[rustfmt::skip]
const US_STATE_FILTER_ABBRS: &[&str] = &[
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "DC", "FL", "GA", "HI", "ID",
    "IL", "IN", "IA", "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO",
    "MT", "NE", "NV", "NH", "NJ", "NM", "NY", "NC", "ND", "OH", "OK", "OR", "PA",
    "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV", "WI", "WY",
];

/// Draw the visibility range as direct choices in the placefile gear menu.
///
/// Do not replace these with a `ComboBox`: both the gear menu and a combo are
/// egui popups, and opening the nested combo closes its owning menu before a
/// range can be selected.
fn placefile_visibility_range_menu(
    ui: &mut egui::Ui,
    visibility_range_percent: &mut u16,
) -> (
    bool,
    [(u16, egui::Response); PLACEFILE_VISIBILITY_RANGE_PERCENTS.len()],
) {
    ui.weak("Visibility when zoomed out");
    let mut changed = false;
    let choices = PLACEFILE_VISIBILITY_RANGE_PERCENTS.map(|percent| {
        let response = ui.selectable_value(
            visibility_range_percent,
            percent,
            crate::placefile_visibility_range_label(percent),
        );
        changed |= response.changed();
        (percent, response)
    });
    ui.weak("Source range respects the file's Threshold statements.");
    (changed, choices)
}

impl ViewerApp {
    /// Add a community URL or downloaded placefile through one slot path.
    /// Selecting an existing source is a useful explicit reload instead of
    /// silently doing nothing.
    fn add_or_reload_placefile_source(&mut self, source: String, show_text_default: bool) {
        if let Some(slot) = self
            .placefile_slots
            .iter_mut()
            .find(|slot| slot.url == source)
        {
            slot.enabled = true;
            slot.next_refresh = Some(Instant::now());
            slot.status = "reload queued".to_owned();
            return;
        }
        let mut slot = PlacefileSlot::new(source, true);
        slot.show_text = show_text_default;
        self.placefile_slots.push(slot);
    }

    /// Honest layer count for the rail header (ui-refresh proposal §1.3.3):
    /// everything the rail shows as an enabled row.
    pub(crate) fn rail_layer_count(&self) -> usize {
        usize::from(self.volume.is_some())
            + self.radar_layers.len()
            + usize::from(self.italy_dpc_layer.is_some())
            + usize::from(self.taiwan_cwa_layer.is_some())
            + usize::from(self.grid_composite_placeholder_source().is_some())
            + usize::from(self.sat_layer.is_some())
            + self.model_layers.len()
            + usize::from(self.obs_enabled)
            + usize::from(self.app_settings.overlay_river_gauges)
            + usize::from(self.glm_enabled)
            + usize::from(self.raob_markers_enabled)
            + usize::from(self.app_settings.show_tropical)
            + usize::from(!self.spc_outlooks_enabled.is_empty())
            + usize::from(self.spc_reports_enabled)
            + usize::from(self.mping_enabled)
            + usize::from(self.swath.reflectivity.enabled)
            + usize::from(self.swath.velocity.enabled)
            + usize::from(self.swath.correlation_coefficient.enabled)
            + usize::from(self.hazards_visible && self.hazard_overlay.is_some())
            + usize::from(self.tor_tracks.show_tracks)
            + usize::from(self.tor_tracks.show_tds)
            + usize::from(self.wofs.drape_on_map)
            + usize::from(self.farm.drape.enabled)
            + self.placefile_slots.iter().filter(|s| s.enabled).count()
    }

    /// A grid composite fetch with no model-layer row yet (the first
    /// fetch) — refreshes of an existing row show on that row instead of
    /// flashing a second "loading" row every auto-refresh.
    fn grid_composite_placeholder_source(&self) -> Option<grid_composites::GridCompositeSource> {
        self.grid_composite_loading.filter(|source| {
            !self.model_layers.iter().any(|slot| {
                grid_composites::GridCompositeSource::from_variable_slug(&slot.layer.field.key.var)
                    == Some(*source)
            })
        })
    }

    /// The rail rows, grouped BASE → ATMOSPHERE → OBS → SEVERE → COMMUNITY
    /// (spec §2.2): primary + overlay radars + rotation tracks/TDS, then
    /// model/OA fields + GOES + drapes, then obs + lightning, then SPC +
    /// warnings, then placefiles.
    pub(crate) fn layers_rail(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Groups are kit subgroup headers, NOT collapsing — the rail stays
        // one scannable list (spec §2.2). Map paint order is unchanged (the
        // compositor is layer-type-major); the grouping is a reading aid.
        let overlay_count = self.radar_layers.len();
        let mut clear_overlays = false;
        panel_kit::subgroup(ui, "Base", |ui| {
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
        // app (badge ⏺ instead); site/products live in the sections
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
        } else if self.primary.live.enabled {
            "live"
        } else {
            "loaded"
        };
        let primary_frame_time = self.selected_frame_scan_time_utc();
        let primary_count = primary_frame_time.as_ref().map(|time| {
            format!(
                "{} · {}",
                time.format("%H:%MZ"),
                compact_status_age(time.to_owned(), chrono::Utc::now())
            )
        });
        let primary_hover = primary_frame_time
            .as_ref()
            .map(|time| {
                format!(
                    "Primary radar (site/products in SITE and PRODUCTS above)\nDisplayed source frame: {} ({})",
                    time.format("%Y-%m-%d %H:%M:%SZ"),
                    compact_status_age(time.to_owned(), chrono::Utc::now())
                )
            })
            .unwrap_or_else(|| "Primary radar (site/products in SITE and PRODUCTS above)\nNo source frame loaded".to_owned());
        if layer_row(
            ui,
            LayerRowSpec {
                vis: LayerRowVis::Badge {
                    glyph: "⏺",
                    hover: "Primary radar — always drawn; site and products in the sections above",
                },
                name: &primary_name,
                name_hover: &primary_hover,
                state: Some(primary_state),
                count: primary_count.as_deref(),
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
        // Max-value swath overlays (peak REF / peak |V| over the loaded loop)
        // are pure products of the radar loop, so they sit with the radars.
        self.max_swath_rail_rows(ui, ctx);
        let has_model_rows = !self.model_layers.is_empty();
        let mut step_hour: i64 = 0;
        // The Hour stepper rides the group header: it steps every
        // dock-following model row at once (spec §2.3).
        panel_kit::subgroup(ui, "Atmosphere", |ui| {
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
        #[derive(Clone, Copy)]
        enum ModelColorAction {
            Auto,
            Use(ColorTableFamily),
            Edit(ColorTableFamily),
        }
        let mut model_color_action: Option<(u64, ModelColorAction)> = None;
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
        let mut refresh_grid_composite: Option<grid_composites::GridCompositeSource> = None;
        let mut center_grid_composite: Option<grid_composites::GridCompositeSource> = None;
        if let Some(source) = self.grid_composite_placeholder_source() {
            let name = source.short_label();
            let name_hover = source.imgw_attribution().map_or_else(
                || source.label(),
                |attribution| format!("{}\n\n{attribution}", source.label()),
            );
            let _ = layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Badge {
                        glyph: "○",
                        hover: "Fetching latest gridded radar composite",
                    },
                    name: &name,
                    name_hover: &name_hover,
                    state: Some("loading"),
                    ..Default::default()
                },
                |ui| {
                    ui.spinner();
                },
            );
        }
        for slot in &mut self.model_layers {
            let id = slot.id;
            let layer = &mut slot.layer;
            let grid_source =
                grid_composites::GridCompositeSource::from_variable_slug(&layer.field.key.var);
            // Raw wrf_* store fields show their friendly catalog label in the
            // rail (same label the model dock picker shows); the hover keeps
            // the real store name plus the catalog's one-line description.
            let wrf_info = color_tables::wrf_field_info(&layer.field.key.var);
            // Synthesized per-level isobaric fields carry their slug
            // (`temperature_850`) — show the picker's label for them too.
            let iso_label = match wrf_info {
                Some(_) => None,
                None => color_tables::parse_iso_slug(&layer.field.key.var).map(|spec| spec.label()),
            };
            let name = grid_source
                .map(grid_composites::GridCompositeSource::short_label)
                .unwrap_or_else(|| {
                    let label = wrf_info
                        .map(|info| info.label)
                        .or(iso_label.as_deref())
                        .unwrap_or(&layer.field.key.var);
                    format!("{} f{:02}", label, layer.field.key.hour.hour)
                });
            let grid_frame_text = grid_source.and_then(|source| {
                grid_composites::frame_text_for(&self.grid_composite_status, source)
            });
            let grid_fetching = grid_source.is_some() && grid_source == self.grid_composite_loading;
            let name_hover = grid_source.map_or_else(
                || match (wrf_info, &iso_label) {
                    (Some(info), _) => format!(
                        "{} ({}) — store field {}\n{}\nLayers draw bottom-to-top in list order\nNewest: {}",
                        info.label,
                        layer.field.units,
                        layer.field.key.var,
                        info.description,
                        newest_run_text
                    ),
                    (None, Some(label)) => format!(
                        "{} ({}) — {} plane from the hour's isobaric sounding volumes\nLayers draw bottom-to-top in list order\nNewest: {}",
                        label, layer.field.units, layer.field.key.var, newest_run_text
                    ),
                    (None, None) => format!(
                        "{} ({}) — layers draw bottom-to-top in list order\nNewest: {}",
                        layer.field.key.var, layer.field.units, newest_run_text
                    ),
                },
                |source| {
                    let mut hover = format!(
                        "{} ({})\nFrame: {}\nLatest public grid composite — auto-refreshes ~60 s while visible; layers draw bottom-to-top in list order",
                        source.label(),
                        layer.field.units,
                        grid_frame_text.as_deref().unwrap_or("pending first fetch"),
                    );
                    if let Some(attribution) = source.imgw_attribution() {
                        hover.push_str("\n\n");
                        hover.push_str(&attribution);
                    }
                    hover
                },
            );
            let mut order_delta: i8 = 0;
            let mut open_window = false;
            let mut remove_this = false;
            let refreshable_source = grid_source;
            let current_color_family = layer.custom_color_family;
            // The two-extra budget at 320 pt: the frame text rides the
            // count column; Refresh and the Color picker live behind ⚙
            // (wave-1 gear-popover precedent) with the window jump as the
            // menu's first entry.
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut layer.visible,
                        hover: model_map_layer_visibility_hover(grid_source.is_some()),
                    },
                    name: &name,
                    name_hover: &name_hover,
                    state: grid_fetching.then_some("fetching"),
                    count: grid_frame_text.as_deref(),
                    opacity: Some(LayerRowOpacity::F32 {
                        value: &mut layer.opacity,
                        min: 0.1,
                        hover: model_map_layer_opacity_hover(grid_source.is_some()),
                    }),
                    order: (model_row_count > 1).then_some(LayerRowOrder {
                        delta: &mut order_delta,
                    }),
                    gear: Some(LayerRowGear::Menu {
                        hover: if grid_source.is_some() {
                            "Grid layer options (center · refresh · colors)"
                        } else {
                            "Model layer options (open window · colors)"
                        },
                        content: Box::new(|ui| {
                            if let Some(source) = refreshable_source {
                                if ui
                                    .button("Center on grid")
                                    .on_hover_text("Center the map on this grid's source region")
                                    .clicked()
                                {
                                    center_grid_composite = Some(source);
                                    ui.close();
                                }
                            } else if ui
                                .button("Open Model window")
                                .on_hover_text("Runs · fields · soundings · download")
                                .clicked()
                            {
                                open_window = true;
                                ui.close();
                            }
                            if refreshable_source.is_some()
                                && ui
                                    .button("Refresh composite")
                                    .on_hover_text("Fetch the latest gridded radar composite")
                                    .clicked()
                            {
                                refresh_grid_composite = refreshable_source;
                                ui.close();
                            }
                            ui.separator();
                            ui.menu_button("Color", |ui| {
                                if ui
                                    .selectable_label(current_color_family.is_none(), "Auto")
                                    .on_hover_text(
                                        "Automatic model colors: WRF/model fields use Solarpower07's WRF-Runner palettes (reflectivity · temp · dew point · wind · precip · RH · CAPE), otherwise Rusty Weather's production style, then a generic ramp.",
                                    )
                                    .clicked()
                                {
                                    model_color_action = Some((id, ModelColorAction::Auto));
                                    ui.close();
                                }
                                ui.separator();
                                for family in ColorTableFamily::ALL {
                                    if ui
                                        .selectable_label(
                                            current_color_family == Some(family),
                                            family.label(),
                                        )
                                        .clicked()
                                    {
                                        model_color_action =
                                            Some((id, ModelColorAction::Use(family)));
                                        ui.close();
                                    }
                                }
                                ui.separator();
                                let edit_family =
                                    current_color_family.unwrap_or(ColorTableFamily::Generic);
                                if ui
                                    .button(format!("Edit {}", edit_family.label()))
                                    .on_hover_text(
                                        "Open Custom > Appearance > Color tables for this family",
                                    )
                                    .clicked()
                                {
                                    model_color_action =
                                        Some((id, ModelColorAction::Edit(edit_family)));
                                    ui.close();
                                }
                            })
                            .response
                            .on_hover_text(
                                "Override this layer with an editable BowEcho color table",
                            );
                        }),
                    }),
                    remove: Some(LayerRowRemove {
                        hover: "Remove this layer",
                        clicked: &mut remove_this,
                    }),
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
        if let Some(source) = center_grid_composite {
            let (latitude, longitude, scale) = source.map_center();
            self.map_center_lat = latitude;
            self.map_center_lon = longitude;
            self.map_scale = scale;
            self.clamp_map_center();
            ctx.request_repaint();
        }
        if let Some((id, action)) = model_color_action {
            let edit_family = match action {
                ModelColorAction::Edit(family) => Some(family),
                _ => None,
            };
            if let Some(slot) = self.model_layers.iter_mut().find(|slot| slot.id == id) {
                match action {
                    ModelColorAction::Auto => {
                        slot.layer.custom_color_family = None;
                        self.status = format!(
                            "{} follows automatic model colors",
                            slot.layer.field.key.var
                        );
                    }
                    ModelColorAction::Use(family) | ModelColorAction::Edit(family) => {
                        slot.layer.custom_color_family = Some(family);
                        self.status = format!(
                            "{} uses {} color table",
                            slot.layer.field.key.var,
                            family.label()
                        );
                    }
                }
                slot.layer.generation = slot.layer.generation.wrapping_add(1);
                slot.texture = None;
                ctx.request_repaint();
            }
            if let Some(family) = edit_family {
                self.request_color_table_manager(family);
            }
        }
        if let Some(source) = refresh_grid_composite {
            self.start_grid_composite_refresh(source, ctx, true);
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
        let dpc_loading =
            self.italy_dpc_latest_rx.in_flight() || self.italy_dpc_render_rx.is_some();
        let mut dpc_product_pick: Option<ItalyDpcMapProduct> = None;
        let mut dpc_refresh = false;
        let mut dpc_remove = false;
        if let Some(layer) = &mut self.italy_dpc_layer {
            let name = format!("Italy DPC {}", layer.product.short_label());
            let frame_text = layer
                .product_time_millis
                .and_then(|millis| chrono::Utc.timestamp_millis_opt(millis).single())
                .map(|time| time.format("%H:%MZ").to_string())
                .unwrap_or_else(|| "latest".to_owned());
            let state = if dpc_loading {
                "loading"
            } else if layer.error.is_some() {
                "stale"
            } else if layer.product_time_millis.is_some() {
                "live"
            } else {
                "loading"
            };
            let name_hover = if let Some(error) = &layer.error {
                format!(
                    "Official Italy DPC {} raw GeoTIFF overlay\nLatest fetch error: {}",
                    layer.product.label(),
                    error
                )
            } else {
                format!(
                    "Official Italy DPC {} raw GeoTIFF overlay\nFrame: {}{}",
                    layer.product.label(),
                    frame_text,
                    layer
                        .period
                        .as_deref()
                        .map(|period| format!(" · {period}"))
                        .unwrap_or_default()
                )
            };
            let current_product = layer.product;
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut layer.visible,
                        hover: "Show Italy DPC raw radar/composite raster on the map",
                    },
                    name: &name,
                    name_hover: &name_hover,
                    state: Some(state),
                    count: Some(&frame_text),
                    opacity: Some(LayerRowOpacity::F32 {
                        value: &mut layer.opacity,
                        min: 0.05,
                        hover: "Italy DPC raw raster opacity",
                    }),
                    gear: Some(LayerRowGear::Menu {
                        hover: "Italy DPC product and refresh",
                        content: Box::new(|ui| {
                            ui.set_min_width(190.0);
                            for product in ItalyDpcMapProduct::ALL {
                                if ui
                                    .selectable_label(product == current_product, product.label())
                                    .clicked()
                                {
                                    dpc_product_pick = Some(product);
                                    ui.close();
                                }
                            }
                            ui.separator();
                            if ui.button("Refresh latest").clicked() {
                                dpc_refresh = true;
                                ui.close();
                            }
                        }),
                    }),
                    remove: Some(LayerRowRemove {
                        hover: "Remove Italy DPC overlay",
                        clicked: &mut dpc_remove,
                    }),
                    ..Default::default()
                },
                |_ui| {},
            ) {
                ctx.request_repaint();
            }
        }
        if let Some(product) = dpc_product_pick {
            self.set_italy_dpc_product(product, ctx);
        } else if dpc_refresh {
            self.start_italy_dpc_latest_refresh(ctx, true);
        }
        if dpc_remove {
            self.italy_dpc_layer = None;
            self.italy_dpc_latest_rx.cancel();
            self.italy_dpc_render_rx = None;
            self.italy_dpc_texture = None;
            ctx.request_repaint();
        }
        let taiwan_fetching = self.taiwan_cwa_latest_rx.in_flight();
        let taiwan_rendering = self.taiwan_cwa_render_rx.is_some();
        let mut taiwan_refresh = false;
        let mut taiwan_remove = false;
        if let Some(layer) = &mut self.taiwan_cwa_layer {
            let frame_text = layer
                .frame_time
                .map(|time| time.format("%H:%MZ").to_string())
                .unwrap_or_else(|| "latest".to_owned());
            let state = if taiwan_fetching {
                "fetching"
            } else if taiwan_rendering {
                "rendering"
            } else if layer.error.is_some() {
                "stale"
            } else if layer.frame_time.is_some() {
                "live"
            } else {
                "loading"
            };
            let name_hover = if let Some(error) = &layer.error {
                format!(
                    "Taiwan CWA O-A0059-001 composite reflectivity\nLatest fetch error: {error}"
                )
            } else {
                format!("Taiwan CWA O-A0059-001 composite reflectivity\nFrame: {frame_text}")
            };
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut layer.visible,
                        hover: "Show Taiwan CWA composite reflectivity on the map",
                    },
                    name: "Taiwan CWA REF",
                    name_hover: &name_hover,
                    state: Some(state),
                    count: Some(&frame_text),
                    opacity: Some(LayerRowOpacity::F32 {
                        value: &mut layer.opacity,
                        min: 0.05,
                        hover: "Taiwan CWA raster opacity",
                    }),
                    gear: Some(LayerRowGear::Menu {
                        hover: "Taiwan CWA layer options",
                        content: Box::new(|ui| {
                            if ui.button("Refresh latest").clicked() {
                                taiwan_refresh = true;
                                ui.close();
                            }
                        }),
                    }),
                    remove: Some(LayerRowRemove {
                        hover: "Remove Taiwan CWA overlay",
                        clicked: &mut taiwan_remove,
                    }),
                    ..Default::default()
                },
                |_ui| {},
            ) {
                ctx.request_repaint();
            }
        }
        if taiwan_refresh {
            self.start_taiwan_cwa_latest_refresh(ctx, true);
        }
        if taiwan_remove {
            self.taiwan_cwa_layer = None;
            self.taiwan_cwa_latest_rx.cancel();
            self.taiwan_cwa_render_rx = None;
            self.taiwan_cwa_texture = None;
            ctx.request_repaint();
        }
        let mut remove_sat_layer = false;
        let mut open_sat_window = false;
        let sat_layer_source =
            self.sat_layer
                .as_ref()
                .map(|layer| match layer.key.model.as_str() {
                    "mtg_i1" => ("Meteosat", "Meteosat-12 / EUMETSAT satellite frame"),
                    "h8" | "h9" => ("Himawari", "Himawari AHI satellite frame"),
                    "simsat" => ("SimSat", "Simulated satellite frame"),
                    _ => ("GOES", "GOES ABI satellite frame"),
                });
        let sat_frame_time = self
            .sat_layer
            .as_ref()
            .and_then(|layer| rw_sat::store::frame_time(&layer.key.run, layer.hhmm));
        let sat_count = self.sat_layer.as_ref().map(|layer| {
            sat_frame_time
                .as_ref()
                .map(|time| {
                    format!(
                        "{} · {}",
                        time.format("%H:%MZ"),
                        compact_status_age(time.to_owned(), chrono::Utc::now())
                    )
                })
                .unwrap_or_else(|| format!("{:02}:{:02}Z", layer.hhmm / 100, layer.hhmm % 100))
        });
        let sat_name_hover = sat_layer_source.map(|source| source.1).map(|source| {
            sat_frame_time
                .as_ref()
                .map(|time| {
                    format!(
                        "{source}\nSource frame: {} ({})",
                        time.format("%Y-%m-%d %H:%M:%SZ"),
                        compact_status_age(time.to_owned(), chrono::Utc::now())
                    )
                })
                .unwrap_or_else(|| format!("{source}\nSource frame time is not dated in this run"))
        });
        let sat_loading = self.sat_map_inflight.is_some()
            || self.sat_map_pending.is_some()
            || self.sat_layer_build_rx.is_some()
            || self.sat_layer_render_rx.is_some();
        if let Some(layer) = &mut self.sat_layer
            && layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut layer.visible,
                        hover: "Show satellite imagery on map",
                    },
                    name: sat_layer_source
                        .map(|source| source.0)
                        .unwrap_or("Satellite"),
                    name_hover: sat_name_hover.as_deref().unwrap_or("Satellite frame"),
                    state: Some(if sat_loading { "loading" } else { "loaded" }),
                    count: sat_count.as_deref(),
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
            // Remove must stick: while "Map follows player" is on, the
            // timeline sync re-requests a map frame every update and the
            // layer pops straight back — so removing the row also drops the
            // follow linkage (with a status note) plus any pending map-frame
            // work, mirroring clear_satellite_display_for_spec_change minus
            // the run listings. sat_last_frame survives so the Sat window's
            // "Show on radar map" can bring the layer right back.
            if self.sat_map_follow {
                self.sat_map_follow = false;
                self.status =
                    "Removed satellite layer · map no longer follows the satellite player"
                        .to_owned();
            }
            self.sat_layer = None;
            self.sat_layer_texture = None;
            self.sat_layer_build_rx = None;
            self.sat_layer_render_rx = None;
            self.sat_map_inflight = None;
            self.sat_map_pending = None;
            ctx.request_repaint();
        }
        // WoFS DRAPE — the row is born from the WoFS window's "Show on map"
        // (the Sat/Model convention) and lives here like every other layer
        // (spec §2.3). Its pump and radar-time sync continue while the window
        // is closed, and the state dot reports actual draw readiness.
        if self.wofs.drape_on_map {
            let minute = self.wofs.minute;
            let init = self.wofs.init.clone();
            let readiness = self.wofs.drape_readiness();
            let name_hover = format!(
                "WoFS ensemble drape: {} · init {}z · f+{}m{}\nGeoreferenced onto the radar map; product/run/minute live in the WoFS window.\nMap status: {}",
                self.wofs.product,
                if init.is_empty() { "??" } else { &init },
                minute,
                if self.wofs.sync_to_radar {
                    " · synced to radar time"
                } else {
                    ""
                },
                readiness.description(),
            );
            let mut open_wofs = false;
            let mut remove_drape = false;
            let mut visibility_mode_changed = false;
            let high_visibility = &mut self.wofs.drape_high_visibility;
            let selection = (!init.is_empty()).then(|| format!("{init}z+{minute}m"));
            let wofs_count = if readiness == crate::wofs::WofsDrapeReadiness::Ready {
                selection
            } else {
                Some(match selection {
                    Some(selection) => format!("{} · {selection}", readiness.rail_state()),
                    None => readiness.rail_state().to_owned(),
                })
            };
            let row_changed = layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.wofs.drape_on_map,
                        hover: "Drape the current WoFS product onto the radar map",
                    },
                    name: "WoFS drape",
                    name_hover: &name_hover,
                    state: Some(readiness.rail_state()),
                    count: wofs_count.as_deref(),
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
                    if crate::wofs::drape_visibility_ui(ui, high_visibility, true) {
                        visibility_mode_changed = true;
                    }
                },
            );
            if row_changed || visibility_mode_changed {
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
            let farm_frame_time = self
                .farm
                .frames
                .get(self.farm.frame_index)
                .and_then(|url| farm_frame_time_utc(url));
            let farm_count = farm_frame_time
                .as_ref()
                .map(|time| {
                    format!(
                        "{} · {}",
                        time.format("%H:%MZ"),
                        compact_status_age(time.to_owned(), chrono::Utc::now())
                    )
                })
                .or_else(|| live.clone());
            let farm_hover = format!(
                "Georeferenced FARM (DOW/COW) quicklook drape\nSensor: {} · Product: {}\n{}{}",
                live.as_deref().unwrap_or("selected deployment"),
                if self.farm.product.is_empty() {
                    "unknown"
                } else {
                    self.farm.product.as_str()
                },
                if self.farm.status.is_empty() {
                    "No completed frame-page status"
                } else {
                    self.farm.status.as_str()
                },
                farm_frame_time
                    .as_ref()
                    .map(|time| format!(
                        "\nDisplayed source frame: {} ({})",
                        time.format("%Y-%m-%d %H:%M:%SZ"),
                        compact_status_age(time.to_owned(), chrono::Utc::now())
                    ))
                    .unwrap_or_default()
            );
            let farm_loading = self.farm.background_activity_label().is_some();
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
                    name_hover: &farm_hover,
                    state: Some(if farm_loading {
                        "loading"
                    } else if live.is_some() {
                        "live"
                    } else {
                        "loaded"
                    }),
                    count: farm_count.as_deref(),
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
                |_ui| {},
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
        panel_kit::subgroup(ui, "Obs", |ui| {
            let _ = ui;
        });
        {
            // Surface obs as a layer row; the network sub-toggles
            // violated the two-extra budget inline, so they live
            // behind the row's ⚙ popover now (spec §2.3).
            let obs_show_metar = &mut self.obs_show_metar;
            let obs_show_mesonet = &mut self.obs_show_mesonet;
            let mut obs_adjust_soundings = self.obs_adjust_soundings;
            let obs_hour_loop_enabled = &mut self.obs_hour_loop_enabled;
            let obs_hour_loop_started_at = &mut self.obs_hour_loop_started_at;
            let obs_hour_loop_paused_at = &mut self.obs_hour_loop_paused_at;
            let obs_hour_loop_end_utc = &mut self.obs_hour_loop_end_utc;
            let obs_fetched_at = [
                self.obs_fetched_at,
                self.iem_metar_fetched_at,
                self.nws_obs_fetched_at,
                self.mesonet_fetched_at,
            ]
            .into_iter()
            .flatten()
            .max();
            let obs_station_count = self.surface_obs.station_count;
            let mut obs_adjust_changed = false;
            let mut metar_state_filter_enabled =
                self.app_settings.overlay_obs_metar_state_filter_enabled;
            let mut metar_states = self.app_settings.overlay_obs_metar_states.clone();
            let mut metar_state_filter_changed = false;
            let obs_fetching = self.obs_rx.is_some()
                || self.iem_metar_rx.is_some()
                || self.nws_obs_rx.is_some()
                || self.mesonet_rx.is_some();
            let obs_success_age =
                obs_fetched_at.map(|at| compact_layer_success_age(at, Instant::now()));
            let obs_count_text = obs_success_age
                .as_deref()
                .map(|age| format!("{obs_station_count} stn · {age}"))
                .or_else(|| (obs_station_count > 0).then(|| format!("{obs_station_count} stn")));
            let obs_hover = format!(
                "METAR station plots: temperature/dewpoint (units per Settings > Display), wind barbs, gusts — every reporting station, refreshed ~5 min\n{}{}",
                obs_success_age
                    .as_deref()
                    .map(|age| format!("Last successful source update: {age}"))
                    .unwrap_or_else(|| "No successful source update yet".to_owned()),
                if self.status.starts_with("Surface obs fetch failed")
                    || self.status.starts_with("IEM global METAR fetch failed")
                {
                    format!("\nLatest refresh status: {}", self.status)
                } else {
                    String::new()
                }
            );
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.obs_enabled,
                        hover: "METAR station plots: temperature/dewpoint (units per Settings > Display), wind barbs, gusts — every reporting station, refreshed ~5 min",
                    },
                    name: "Surface obs",
                    name_hover: &obs_hover,
                    state: Some(if obs_fetching {
                        "loading"
                    } else if self.status.starts_with("Surface obs fetch failed")
                        || self.status.starts_with("IEM global METAR fetch failed")
                    {
                        "error"
                    } else if obs_fetched_at.is_some() {
                        "live"
                    } else {
                        "idle"
                    }),
                    count: obs_count_text.as_deref(),
                    gear: Some(LayerRowGear::PersistentMenu {
                        hover: "Networks: METAR · Mesonet · obs-adjusted soundings",
                        content: Box::new(|ui| {
                            ui.set_min_width(370.0);
                            ui.checkbox(obs_show_metar, "METAR")
                                .on_hover_text("Airport-grade ASOS/AWOS stations");
                            ui.checkbox(obs_show_mesonet, "Mesonet")
                                        .on_hover_text(
                                            "IEM RWIS road sensors + DCP/RAWS networks — denser but lower siting quality (road sensors read hot in sun); uncheck for strict-QC METAR-only",
                                        );
                            let state_filter_label = if metar_state_filter_enabled {
                                format!("Station states ({})", metar_states.len())
                            } else {
                                "Station states (all)".to_owned()
                            };
                            egui::CollapsingHeader::new(state_filter_label)
                                .default_open(metar_state_filter_enabled)
                                .show(ui, |ui| {
                                if ui
                                    .checkbox(
                                        &mut metar_state_filter_enabled,
                                        "Limit station plots by state",
                                    )
                                    .on_hover_text(
                                        "Display-only filter for METAR and mesonet station plots. Model analysis and obs-adjusted soundings still use every loaded observation.",
                                    )
                                    .changed()
                                {
                                    metar_state_filter_changed = true;
                                }
                                ui.horizontal(|ui| {
                                    if ui.button("All states").clicked() {
                                        metar_state_filter_enabled = false;
                                        metar_state_filter_changed = true;
                                    }
                                    if ui.button("None").clicked() {
                                        metar_state_filter_enabled = true;
                                        metar_states.clear();
                                        metar_state_filter_changed = true;
                                    }
                                });
                                ui.add_enabled_ui(metar_state_filter_enabled, |ui| {
                                    egui::Grid::new("metar_state_filter_grid")
                                        .num_columns(7)
                                        .spacing(egui::vec2(6.0, 2.0))
                                        .show(ui, |ui| {
                                            for (index, abbreviation) in
                                                US_STATE_FILTER_ABBRS.iter().enumerate()
                                            {
                                                let abbreviation = *abbreviation;
                                                let mut selected = metar_states
                                                    .iter()
                                                    .any(|state| state == abbreviation);
                                                if ui
                                                    .checkbox(&mut selected, abbreviation)
                                                    .changed()
                                                {
                                                    if selected {
                                                        metar_states.push(abbreviation.to_owned());
                                                        metar_states.sort();
                                                        metar_states.dedup();
                                                    } else {
                                                        metar_states
                                                            .retain(|state| state != abbreviation);
                                                    }
                                                    metar_state_filter_changed = true;
                                                }
                                                if (index + 1) % 7 == 0 {
                                                    ui.end_row();
                                                }
                                            }
                                        });
                                    if metar_states.is_empty() {
                                        ui.weak("No states selected: all station plots are hidden.");
                                    }
                                });
                            });
                            if ui
                                .checkbox(obs_hour_loop_enabled, "Loop latest hour")
                                .on_hover_text(
                                    "Animate the latest hour of surface observations in 30 seconds. METARs are usually hourly; mesonet/RWIS/RAWS reports are often denser.",
                                )
                                .changed()
                            {
                                if *obs_hour_loop_enabled {
                                    *obs_hour_loop_started_at = Instant::now();
                                    *obs_hour_loop_paused_at = None;
                                    *obs_hour_loop_end_utc = chrono::Utc::now();
                                } else {
                                    *obs_hour_loop_paused_at = None;
                                }
                                ui.ctx().request_repaint();
                            }
                            if *obs_hour_loop_enabled && ui.button("Restart obs loop").clicked() {
                                *obs_hour_loop_started_at = Instant::now();
                                *obs_hour_loop_paused_at = None;
                                *obs_hour_loop_end_utc = chrono::Utc::now();
                                ui.ctx().request_repaint();
                            }
                            ui.separator();
                            ui.weak("Station plot: red T, green Td, wind barb.");
                            ui.weak("Dots: white METAR, amber mesonet.");
                            ui.separator();
                            if ui
                                        .checkbox(&mut obs_adjust_soundings, "Obs-adjusted soundings")
                                        .on_hover_text(
                                            "The skew-T's surface T/Td/wind come from the nearest station (within 30 km, fresher than 60 min) instead of the model — parcels recompute from the REAL surface. The title shows which station adjusted it.",
                                        )
                                        .changed()
                                    {
                                        obs_adjust_changed = true;
                                        ui.ctx().request_repaint();
                                    }
                            ui.separator();
                            ui.weak("Appearance controls land here next.");
                            ui.separator();
                            if ui.button("Done").clicked() {
                                ui.close();
                            }
                        }),
                    }),
                    ..Default::default()
                },
                |ui| {
                    if obs_fetching {
                        ui.spinner();
                    }
                },
            ) {
                ctx.request_repaint();
            }
            if metar_state_filter_changed {
                self.app_settings.overlay_obs_metar_state_filter_enabled =
                    metar_state_filter_enabled;
                self.app_settings.overlay_obs_metar_states = metar_states;
                let _ = self.app_settings.save();
                ctx.request_repaint();
            }
            if obs_adjust_changed {
                // Match the top-of-sounding shortcut: adjustment depends on
                // Surface obs, while turning adjustment back off must not
                // unexpectedly hide that independently useful map layer.
                self.set_obs_adjust_soundings(obs_adjust_soundings, ctx);
            }
        }
        // Official NOAA/NWS National Water Prediction Service gauges. Map
        // summaries are viewport-tiled; details and hydrographs load only
        // after a marker click.
        {
            let status = self.river_gauges.status_line();
            let fetching = self.river_gauges.is_fetching();
            let mut refresh = false;
            let mut state_filter_enabled =
                self.app_settings.overlay_river_gauge_state_filter_enabled;
            let mut states = self.app_settings.overlay_river_gauge_states.clone();
            let mut state_filter_changed = false;
            let changed = layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.app_settings.overlay_river_gauges,
                        hover: "Official NOAA/NWS NWPS river gauges. Markers show observed or forecast flood category; click one for stage, flow, thresholds, forecast crest, impacts, and hydrograph.",
                    },
                    name: "River gauges",
                    name_hover: "National Water Prediction Service observations and official NWS river forecasts (anonymous, no key)",
                    count: Some(status.as_str()),
                    gear: Some(LayerRowGear::PersistentMenu {
                        hover: "NWPS source, state filter, legend, and refresh",
                        content: Box::new(|ui| {
                            ui.set_min_width(370.0);
                            ui.strong("NOAA/NWS National Water Prediction Service");
                            ui.weak("Viewport-cached; visible map tiles refresh every 5 minutes.");
                            ui.weak("Blue no flooding - yellow action - orange minor - red moderate - magenta major.");
                            ui.weak("An amber outer ring means the displayed category comes from the forecast.");
                            ui.weak("Gray means stale, not current, or unavailable. Values can be provisional.");
                            let state_filter_label = if state_filter_enabled {
                                format!("Gauge states ({})", states.len())
                            } else {
                                "Gauge states (all)".to_owned()
                            };
                            egui::CollapsingHeader::new(state_filter_label)
                                .default_open(state_filter_enabled)
                                .show(ui, |ui| {
                                    if ui
                                        .checkbox(
                                            &mut state_filter_enabled,
                                            "Limit gauge markers by state",
                                        )
                                        .on_hover_text(
                                            "Display-only filter. Cached NWPS data is retained so changing states is immediate.",
                                        )
                                        .changed()
                                    {
                                        state_filter_changed = true;
                                    }
                                    ui.horizontal(|ui| {
                                        if ui.button("All states").clicked() {
                                            state_filter_enabled = false;
                                            state_filter_changed = true;
                                        }
                                        if ui.button("None").clicked() {
                                            state_filter_enabled = true;
                                            states.clear();
                                            state_filter_changed = true;
                                        }
                                    });
                                    ui.add_enabled_ui(state_filter_enabled, |ui| {
                                        egui::Grid::new("river_gauge_state_filter_grid")
                                            .num_columns(7)
                                            .spacing(egui::vec2(6.0, 2.0))
                                            .show(ui, |ui| {
                                                for (index, abbreviation) in
                                                    US_STATE_FILTER_ABBRS.iter().enumerate()
                                                {
                                                    let abbreviation = *abbreviation;
                                                    let mut selected = states
                                                        .iter()
                                                        .any(|state| state == abbreviation);
                                                    if ui
                                                        .checkbox(&mut selected, abbreviation)
                                                        .changed()
                                                    {
                                                        if selected {
                                                            states.push(abbreviation.to_owned());
                                                            states.sort();
                                                            states.dedup();
                                                        } else {
                                                            states.retain(|state| {
                                                                state != abbreviation
                                                            });
                                                        }
                                                        state_filter_changed = true;
                                                    }
                                                    if (index + 1) % 7 == 0 {
                                                        ui.end_row();
                                                    }
                                                }
                                            });
                                        if states.is_empty() {
                                            ui.weak("No states selected: all gauge markers are hidden.");
                                        }
                                    });
                                });
                            ui.separator();
                            ui.hyperlink_to("Official NWPS", "https://water.noaa.gov/");
                            if ui.button("Refresh visible gauges").clicked() {
                                refresh = true;
                            }
                            if ui.button("Done").clicked() {
                                ui.close();
                            }
                        }),
                    }),
                    ..Default::default()
                },
                |ui| {
                    if fetching {
                        ui.spinner();
                    }
                },
            );
            if refresh {
                self.river_gauges.request_refresh();
                ctx.request_repaint();
            }
            if state_filter_changed {
                self.app_settings.overlay_river_gauge_state_filter_enabled = state_filter_enabled;
                self.app_settings.overlay_river_gauge_states = states;
                let _ = self.app_settings.save();
                ctx.request_repaint();
            }
            if changed {
                self.save_overlay_defaults();
                ctx.request_repaint();
            }
        }
        // LIGHTNING (GLM) — promoted from a bare checkbox to the row
        // grammar (spec §2.3). No opacity in v1: age-fade is
        // intrinsic to the layer.
        {
            let glm_window_min = i64::from(self.app_settings.glm_show_last_minutes.clamp(1, 60));
            let live_ms = chrono::Utc::now().timestamp_millis();
            let frame_ms = self.glm_display_time_ms(live_ms);
            let displaying_live_time = frame_ms == live_ms;
            let glm_source = self.desired_glm_satellite();
            let glm_slots = [self.glm.as_ref(), self.glm_secondary.as_ref()];
            let has_glm_worker = glm_slots.iter().any(|slot| slot.is_some());
            let has_live_glm = glm_slots
                .iter()
                .flatten()
                .filter_map(|glm| glm.latest_flash_time_ms)
                .any(|latest_ms| glm_latest_is_live(live_ms, latest_ms));
            let has_stale_glm = glm_slots.iter().flatten().any(|glm| {
                glm.latest_flash_time_ms.is_some()
                    || glm.last_read_count > 0
                    || glm.fetched_at.is_some()
            });
            if has_live_glm {
                self.glm_refresh_requested_at = None;
                self.glm_ignore_before_ms = None;
            }
            let refresh_elapsed = self
                .glm_refresh_requested_at
                .map(|started| started.elapsed());
            let refresh_active = self.glm_enabled
                && refresh_elapsed.is_some_and(|elapsed| elapsed < Duration::from_secs(45))
                && !has_live_glm;
            let glm_state: Option<&'static str> = if self.glm_enabled {
                if refresh_active {
                    Some("loading")
                } else if has_live_glm {
                    Some("live")
                } else if has_glm_worker && has_stale_glm {
                    Some("stale")
                } else if has_glm_worker || glm_source.is_some() {
                    Some("loading")
                } else {
                    Some("outside")
                }
            } else {
                None
            };
            let glm_line = if self.glm_enabled {
                let lines = glm_slots
                    .into_iter()
                    .flatten()
                    .map(|glm| {
                        let count = glm.frame_flashes(frame_ms, glm_window_min).count();
                        let source = glm_satellite_label(&glm.satellite);
                        if count > 0 && !displaying_live_time {
                            format!("{source} {count} at selected time")
                        } else if count > 0 {
                            format!("{source} live ({count}/{glm_window_min}m)")
                        } else if let Some(error) = &glm.last_read_error {
                            format!("{source} read {}", compact_layer_status(error, 18))
                        } else if let Some(latest_ms) = glm.latest_flash_time_ms {
                            let latest_count = glm.frame_flashes(latest_ms, glm_window_min).count();
                            let age_min = glm_latest_age_minutes(live_ms, latest_ms);
                            if glm_latest_is_live(live_ms, latest_ms) {
                                if displaying_live_time {
                                    format!("{source} live ({latest_count}/{glm_window_min}m)")
                                } else {
                                    format!("{source} live · none at selected time")
                                }
                            } else {
                                format!("{source} stale {age_min}m ago")
                            }
                        } else if glm.last_read_count > 0 {
                            format!("{source} stale")
                        } else if glm.fetched_at.is_some() {
                            let status = compact_layer_status(&glm.last_status, 34);
                            format!("{source} no flashes in last {glm_window_min}m · {status}")
                        } else {
                            format!("{source} {}", compact_layer_status(&glm.last_status, 20))
                        }
                    })
                    .collect::<Vec<_>>();
                let refresh_note = refresh_elapsed
                    .filter(|elapsed| !has_live_glm && *elapsed < Duration::from_secs(45))
                    .map(|elapsed| format!("refreshing {}s", elapsed.as_secs()));
                let no_live_note = refresh_elapsed
                    .filter(|elapsed| !has_live_glm && *elapsed >= Duration::from_secs(45))
                    .map(|_| "no live GLM yet".to_owned());
                let refresh_note = refresh_note.or(no_live_note);
                if !lines.is_empty() {
                    if let Some(note) = refresh_note {
                        format!("{note} · {}", lines.join(" | "))
                    } else {
                        lines.join(" | ")
                    }
                } else if let Some(source) = glm_source {
                    if let Some(note) = refresh_note {
                        format!("{note} · {} starting", glm_satellite_label(source))
                    } else {
                        format!("{} starting", glm_satellite_label(source))
                    }
                } else {
                    "outside GOES view".to_owned()
                }
            } else {
                String::new()
            };
            let source_note = if let Some(source) = glm_source {
                format!(
                    "Source: {} GLM, selected from the current map longitude.",
                    glm_satellite_label(source)
                )
            } else {
                "Source: GOES GLM is unavailable for this map longitude.".to_owned()
            };
            let show_glm_line = self.glm_enabled;
            let mut refresh_glm = false;
            let mut open_mtg_lightning = false;
            let mut quick_mtg_lightning = false;
            let mut next_window_min = glm_window_min;
            let mut glm_style_changed = false;
            let row_changed = layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.glm_enabled,
                        hover: "GOES GLM flashes, free via AWS (no key): trailing Show last window, age-faded, time-synced to the radar loop. Live/stale health uses the newest-flash gate.",
                    },
                    name: "Lightning",
                    name_hover: "GOES GLM point flashes. The gear also opens a Meteosat Lightning Imager accumulated-flash-area loop for Europe/Africa. Himawari has no onboard lightning mapper.",
                    state: glm_state,
                    count: show_glm_line.then_some(glm_line.as_str()),
                    gear: Some(LayerRowGear::Menu {
                        hover: "Lightning data status",
                        content: Box::new(|ui| {
                            ui.weak(source_note);
                            ui.weak(format!(
                                "Live means newest GLM flash is {GLM_LIVE_MAX_AGE_MINUTES} minutes old or newer."
                            ));
                            ui.weak(
                                "Older data is stale. Use Refresh data if it does not catch up.",
                            );
                            ui.weak(
                                "Loaded loops read the loop time span from the rolling GLM store.",
                            );
                            ui.weak(
                                "Meteosat LI is a five-minute accumulated-flash-area raster, not individual point flashes.",
                            );
                            if ui.button("Meteosat LI · 1-hour loop").clicked() {
                                open_mtg_lightning = true;
                                ui.close();
                            }
                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.label("Show last");
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut next_window_min)
                                            .range(1..=60)
                                            .speed(1.0)
                                            .suffix(" min"),
                                    )
                                    .on_hover_text(
                                        "Display flashes from the last N minutes. This does not change the live/stale health rule.",
                                    )
                                    .changed()
                                {
                                    glm_style_changed = true;
                                }
                            });
                            if ui.button("Refresh data").clicked() {
                                refresh_glm = true;
                                ui.close();
                            }
                        }),
                    }),
                    ..Default::default()
                },
                |ui| {
                    if ui
                        .small_button("MTG LI")
                        .on_hover_text(
                            "Load a one-hour Meteosat Lightning Imager accumulated-flash-area loop",
                        )
                        .clicked()
                    {
                        quick_mtg_lightning = true;
                    }
                },
            );
            if refresh_glm {
                self.refresh_glm_data(ctx);
                ctx.request_repaint();
            }
            if open_mtg_lightning || quick_mtg_lightning {
                self.show_satellite = true;
                self.queue_meteosat_product(ctx, eumetsat::MtgProduct::LightningAfa, 12);
                ctx.request_repaint();
            }
            if glm_style_changed {
                self.app_settings.glm_show_last_minutes = next_window_min.clamp(1, 60) as u16;
                self.mark_app_settings_dirty();
                ctx.request_repaint();
            }
            if row_changed {
                self.save_overlay_defaults();
                ctx.request_repaint();
            }
        }
        // RAOB STATIONS — radiosonde launch sites as click-to-sound map
        // markers (field request: "show 21z ILX yesterday in sharppy
        // format" — scrub the loop to the time, click the station).
        {
            let site_count = self.raob_marker_sites().len();
            let fetching_list = self.raob_sites_rx.is_some();
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.raob_markers_enabled,
                        hover: "Radiosonde launch sites as lavender diamond markers — click one for that station's observed sounding at the DISPLAYED radar time (06/18z and other specials included). IEM archive, no key.",
                    },
                    name: "RAOB stations",
                    name_hover: "Observed-sounding launch sites (click a marker for the sounding at the displayed time, rendered in the native skew-T)",
                    gear: Some(LayerRowGear::Menu {
                        hover: "RAOB layer notes",
                        content: Box::new(move |ui| {
                            ui.weak(format!("{site_count} launch sites (live + archive)."));
                            ui.weak("Soundings come from the IEM RAOB archive");
                            ui.weak("(every available launch, including 06/18z");
                            ui.weak("and other specials). The launch nearest BEFORE");
                            ui.weak("the displayed frame time is fetched.");
                        }),
                    }),
                    ..Default::default()
                },
                |ui| {
                    if fetching_list {
                        ui.spinner();
                    }
                },
            ) {
                self.save_overlay_defaults();
                ctx.request_repaint();
            }
        }
        // Anonymous NOAA/NWS MADIS aircraft profiles. This is intentionally
        // named as the limited public MADIS subset, not unrestricted AMDAR.
        {
            let profile_count = self.aircraft_profiles.len();
            let fetching = self.aircraft_profiles_rx.is_some();
            let status = self.aircraft_profiles_status.clone();
            let aircraft_success_age = self
                .aircraft_profiles_fetched_at
                .map(|at| compact_layer_success_age(at, Instant::now()));
            let aircraft_count = match (profile_count, aircraft_success_age.as_deref()) {
                (0, Some(age)) => Some(format!("0 apt · {age}")),
                (count, Some(age)) => Some(format!("{count} apt · {age}")),
                (count, None) if count > 0 => Some(format!("{count} apt")),
                _ => None,
            };
            let aircraft_hover = format!(
                "MADIS aircraft profiles - limited anonymous public real-time subset\n{}\n{}",
                status,
                aircraft_success_age
                    .as_deref()
                    .map(|age| format!("Last successful profile update: {age}"))
                    .unwrap_or_else(|| "No successful profile update yet".to_owned())
            );
            let source_file = self.aircraft_profiles_file.clone();
            let selected_airport = self.selected_aircraft_profile.clone();
            let selected_profile = selected_airport
                .as_deref()
                .and_then(|selected| {
                    self.aircraft_profiles
                        .iter()
                        .find(|profile| profile.airport == selected)
                })
                .map(|profile| {
                    let (latitude, longitude) = profile.marker_position();
                    (
                        profile.display_id(),
                        profile.airport.clone(),
                        latitude,
                        longitude,
                    )
                });
            let newest_profile = self
                .aircraft_profiles
                .iter()
                .max_by(|left, right| left.valid_time.cmp(&right.valid_time))
                .map(|profile| {
                    let (latitude, longitude) = profile.marker_position();
                    (profile.airport.clone(), latitude, longitude)
                });
            let mut follow_selected = self.aircraft_follow_selected;
            let mut follow_changed = false;
            let mut center_profile = None;
            let mut open_profile = None;
            let mut aircraft_search = self.aircraft_profile_search.clone();
            let searchable_profiles = &self.aircraft_profiles;
            let history_loading = self.aircraft_history_rx.is_some();
            let history_count = self.aircraft_history_profiles.len();
            let mut open_history = false;
            let row_changed = layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.aircraft_soundings_enabled,
                        hover: "Latest QC-usable airport ascent/descent profiles from the anonymous public NOAA/NWS MADIS aircraft subset. Click a cyan profile marker to open the native sounding.",
                    },
                    name: "Aircraft soundings (AMDAR/ACARS)",
                    name_hover: &aircraft_hover,
                    state: Some(if fetching {
                        "loading"
                    } else if status.contains("failed") || status.contains("stopped") {
                        "error"
                    } else if self.aircraft_profiles_fetched_at.is_some() {
                        "live"
                    } else {
                        "idle"
                    }),
                    count: aircraft_count.as_deref(),
                    gear: Some(LayerRowGear::PersistentMenu {
                        hover: "MADIS aircraft-profile source and coverage",
                        content: Box::new(|ui| {
                            ui.set_min_width(390.0);
                            ui.strong(format!("{profile_count} current airport profiles"));
                            ui.weak(&status);
                            if let Some(file) = &source_file {
                                ui.weak(format!("Hourly source file: {file}"));
                            }
                            ui.separator();
                            ui.label("Find a current profile");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut aircraft_search)
                                        .hint_text("Airport code, source, ascent/descent…")
                                        .desired_width(270.0),
                                );
                                if !aircraft_search.is_empty() && ui.small_button("Clear").clicked()
                                {
                                    aircraft_search.clear();
                                }
                            });
                            let matching_profiles = searchable_profiles
                                .iter()
                                .filter(|profile| {
                                    aircraft_soundings::profile_matches_search(
                                        profile,
                                        &aircraft_search,
                                    )
                                })
                                .collect::<Vec<_>>();
                            ui.weak(format!(
                                "{} of {} current profiles",
                                matching_profiles.len(),
                                searchable_profiles.len()
                            ));
                            egui::ScrollArea::vertical()
                                .id_salt("aircraft_current_profile_search")
                                .max_height(170.0)
                                .show(ui, |ui| {
                                    for profile in matching_profiles.iter().take(50) {
                                        ui.horizontal(|ui| {
                                            ui.label(format!(
                                                "{} · {} · {}Z",
                                                profile.airport,
                                                profile.direction_label(),
                                                profile.valid_time.format("%H:%M")
                                            ));
                                            if ui.small_button("Center").clicked() {
                                                let (latitude, longitude) =
                                                    profile.marker_position();
                                                center_profile = Some((
                                                    profile.airport.clone(),
                                                    latitude,
                                                    longitude,
                                                ));
                                            }
                                            if ui.small_button("Open").clicked() {
                                                open_profile = Some((**profile).clone());
                                            }
                                        });
                                    }
                                });
                            if ui
                                .add_enabled(
                                    newest_profile.is_some(),
                                    egui::Button::new("Find newest profile"),
                                )
                                .on_hover_text(
                                    "Select and center the newest anonymous MADIS profile endpoint",
                                )
                                .clicked()
                                && let Some(profile) = &newest_profile
                            {
                                center_profile = Some(profile.clone());
                            }
                            if ui
                                .add_enabled(
                                    !history_loading,
                                    egui::Button::new(if history_count > 0 {
                                        format!("Previous soundings ({history_count})")
                                    } else {
                                        "Load previous soundings".to_owned()
                                    }),
                                )
                                .on_hover_text(
                                    "Open a newest-first browser populated on demand from the preceding six public MADIS hourly files",
                                )
                                .clicked()
                            {
                                open_history = true;
                            }
                            if history_loading {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.weak("Loading recent profile history...");
                                });
                            }
                            if let Some((display_id, airport, latitude, longitude)) =
                                &selected_profile
                            {
                                ui.separator();
                                ui.label(format!("Selected: {display_id}"));
                                if ui
                                    .checkbox(&mut follow_selected, "Follow selected profile")
                                    .on_hover_text(
                                        "Recenter on this airport's newest hourly profile endpoint after each MADIS refresh. This is not continuous live aircraft tracking.",
                                    )
                                    .changed()
                                {
                                    follow_changed = true;
                                }
                                if ui.button("Center selected profile").clicked() {
                                    center_profile = Some((airport.clone(), *latitude, *longitude));
                                }
                            } else if let Some(airport) = selected_airport.as_deref() {
                                ui.separator();
                                ui.label(format!("Selected: {airport} (not in latest snapshot)"));
                                if ui
                                    .checkbox(&mut follow_selected, "Follow selected profile")
                                    .on_hover_text(
                                        "Keep waiting for this airport's next hourly profile, or turn follow off here. This is not continuous live aircraft tracking.",
                                    )
                                    .changed()
                                {
                                    follow_changed = true;
                                }
                                ui.weak("The selected profile path is temporarily unavailable.");
                            } else {
                                ui.weak("Click a cyan profile marker to select its path.");
                            }
                            ui.separator();
                            ui.label(format!("Source: {}.", aircraft_soundings::SOURCE_NAME));
                            ui.hyperlink_to(
                                "Official MADIS aircraft documentation",
                                aircraft_soundings::SOURCE_URL,
                            );
                            ui.weak("Coverage is the anonymous public acarsProfiles subset (primarily WVSS-II-equipped aircraft), not unrestricted global AMDAR/ACARS. Restricted airline observations are delayed before public release, so live coverage is sparse and uneven.");
                            ui.weak("MADIS pressure is not present in this feed. BowEcho derives display pressure from the transmitted pressure altitude using the standard atmosphere and labels it as derived.");
                        }),
                    }),
                    ..Default::default()
                },
                |ui| {
                    if fetching {
                        ui.spinner();
                    }
                },
            );
            self.aircraft_profile_search = aircraft_search;
            if let Some((airport, latitude, longitude)) = center_profile {
                self.selected_aircraft_profile = Some(airport);
                self.center_map_on(latitude, longitude);
                ctx.request_repaint();
            }
            if let Some(profile) = open_profile {
                self.start_aircraft_sounding_for(profile, ctx);
            }
            if open_history {
                self.request_aircraft_history(ctx);
            }
            if follow_changed {
                self.aircraft_follow_selected = follow_selected;
                if follow_selected && let Some((_, _, latitude, longitude)) = selected_profile {
                    self.center_map_on(latitude, longitude);
                }
                ctx.request_repaint();
            }
            if row_changed {
                if self.aircraft_soundings_enabled {
                    self.aircraft_profiles_next_poll = None;
                }
                self.save_overlay_defaults();
                ctx.request_repaint();
            }
        }
        panel_kit::subgroup(ui, "Severe", |ui| {
            let _ = ui;
        });
        // Active tropical cyclones already draw official forecast tracks on
        // the map, but their only visibility control used to live deep in
        // Settings. Surface the existing layer here without inventing a
        // second toggle or feed: NHC owns Atlantic and East/Central Pacific,
        // while JTWC supplies the official forecast outside NHC's basins.
        {
            let storm_count = self.tropical.storms.len();
            let fetching = self.tropical.is_fetching();
            let tropical_status = self.tropical.status.clone();
            let tropical_count = (storm_count > 0).then(|| format!("{storm_count} active"));
            let tropical_state = if fetching {
                "loading"
            } else if tropical_status.starts_with("Sources unavailable") {
                "error"
            } else if storm_count > 0 {
                "live"
            } else if tropical_status.starts_with("No active") {
                "quiet"
            } else {
                "idle"
            };
            let tropical_hover = format!(
                "Active tropical-cyclone positions, forecast tracks, intensity, and wind radii. NHC covers the Atlantic and East/Central Pacific; JTWC covers the West Pacific, Indian Ocean, and Southern Hemisphere.\n{tropical_status}"
            );
            // TropicalState keeps storms strongest-first.
            let strongest = self
                .tropical
                .storms
                .first()
                .map(|storm| (storm.name.clone(), storm.position.lat, storm.position.lon));
            let mut show_tracks = self.app_settings.show_tropical;
            let mut show_cards = self.app_settings.show_tropical_panel;
            let mut cards_changed = false;
            let mut center_request = None;
            let row_changed = layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut show_tracks,
                        hover: "Show active tropical-cyclone forecast tracks and wind footprints on the map",
                    },
                    name: "Tropical cyclone tracks",
                    name_hover: &tropical_hover,
                    state: Some(tropical_state),
                    count: tropical_count.as_deref(),
                    gear: Some(LayerRowGear::PersistentMenu {
                        hover: "Tropical-track status, storm cards, and source coverage",
                        content: Box::new(|ui| {
                            ui.set_min_width(360.0);
                            ui.strong("Tropical cyclone tracks");
                            ui.weak(&tropical_status);
                            if ui
                                .checkbox(&mut show_cards, "Storm cards window")
                                .on_hover_text(
                                    "Show the floating active-storm cards while the map layer is enabled",
                                )
                                .changed()
                            {
                                cards_changed = true;
                            }
                            if let Some((name, latitude, longitude)) = &strongest
                                && ui.button(format!("Center {name}")).clicked()
                            {
                                center_request = Some((name.clone(), *latitude, *longitude));
                            }
                            ui.separator();
                            ui.weak("NHC: Atlantic and East/Central Pacific.");
                            ui.weak("JTWC: West Pacific, Indian Ocean, and Southern Hemisphere; GDACS supplies global discovery and supporting geometry.");
                            if ui.button("Done").clicked() {
                                ui.close();
                            }
                        }),
                    }),
                    ..Default::default()
                },
                |ui| {
                    if fetching {
                        ui.spinner();
                    }
                },
            );
            if row_changed || cards_changed {
                self.app_settings.show_tropical = show_tracks;
                self.app_settings.show_tropical_panel = show_cards;
                self.mark_app_settings_dirty();
                ctx.request_repaint();
            }
            if let Some((name, latitude, longitude)) = center_request {
                self.center_map_on(latitude, longitude);
                self.status = format!("Centered on tropical cyclone {name}");
                ctx.request_repaint();
            }
        }
        // SPC OUTLOOK — one row (spec §2.3): vis = any kind enabled
        // (off remembers the set, on restores it); ⚙ jumps to the SEVERE
        // tab's SPC outlooks section (day + kinds live there now).
        {
            let mut spc_on = !self.spc_outlooks_enabled.is_empty();
            let has_estofex = self
                .spc_outlooks_enabled
                .iter()
                .any(|kind| kind == spc_layers::ESTOFEX_OUTLOOK_KIND);
            let has_spc = self
                .spc_outlooks_enabled
                .iter()
                .any(|kind| kind != spc_layers::ESTOFEX_OUTLOOK_KIND);
            let name = match (has_spc, has_estofex) {
                (true, true) => "SPC/ESTOFEX outlooks".to_owned(),
                (false, true) => "ESTOFEX outlook".to_owned(),
                _ => format!("SPC D{} outlook", self.spc_day),
            };
            let fetching = self.spc_rx.is_some();
            let outlook_area_count = self
                .spc_data
                .outlooks
                .iter()
                .map(|(_, features)| features.len())
                .sum::<usize>()
                + self
                    .spc_data
                    .estofex_issues
                    .iter()
                    .map(|issue| issue.polygons.len())
                    .sum::<usize>();
            let spc_completed_age = self
                .spc_data
                .fetched_at
                .map(|at| compact_layer_success_age(at, Instant::now()));
            let outlook_count = spc_completed_age
                .as_deref()
                .map(|age| format!("{outlook_area_count} area · {age}"))
                .or_else(|| (outlook_area_count > 0).then(|| format!("{outlook_area_count} area")));
            let outlook_hover = format!(
                "SPC convective outlook polygons. Off remembers your kind set; on restores it. ⚙ opens the ALERTS tab's SPC outlooks section (day + kinds).\n{}",
                spc_completed_age
                    .as_deref()
                    .map(|age| format!("Last completed SPC check: {age}"))
                    .unwrap_or_else(|| "No completed SPC check yet".to_owned())
            );
            let mut open_severe = false;
            let vis_changed = layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut spc_on,
                        hover: "Convective outlooks in SPC's own colors, archive-aware: shows the displayed day's outlook",
                    },
                    name: &name,
                    name_hover: &outlook_hover,
                    state: Some(if fetching {
                        "loading"
                    } else if self.spc_data.fetched_at.is_some() {
                        "loaded"
                    } else {
                        "idle"
                    }),
                    count: outlook_count.as_deref(),
                    gear: Some(LayerRowGear::Open {
                        hover: "Configure in the ALERTS tab: day · categorical / tornado / wind / hail",
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
                self.invalidate_spc_fetch_request();
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
            let fetching = self.spc_rx.is_some();
            let spc_completed_age = self
                .spc_data
                .fetched_at
                .map(|at| compact_layer_success_age(at, Instant::now()));
            let report_count = self.spc_data.reports.len();
            let reports_count = spc_completed_age
                .as_deref()
                .map(|age| format!("{report_count} rpt · {age}"))
                .or_else(|| (report_count > 0).then(|| format!("{report_count} rpt")));
            let reports_hover = format!(
                "SPC storm report dots + tornado track lines for the displayed day; click a track to load its radar loop\n{}",
                spc_completed_age
                    .as_deref()
                    .map(|age| format!("Last completed SPC check: {age}"))
                    .unwrap_or_else(|| "No completed SPC check yet".to_owned())
            );
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.spc_reports_enabled,
                        hover: "Filtered storm reports (tornado / wind / hail) for the DISPLAYED day: live today (refreshed ~5 min), the archived convective day (12Z-12Z) when browsing the past — with clickable tornado tracks (DATA tab: Event day)",
                    },
                    name: "SPC reports",
                    name_hover: &reports_hover,
                    state: Some(if fetching {
                        "loading"
                    } else if self.spc_data.fetched_at.is_some() {
                        "loaded"
                    } else {
                        "idle"
                    }),
                    count: reports_count.as_deref(),
                    gear: Some(LayerRowGear::Open {
                        hover: "Open the ALERTS tab",
                        clicked: &mut open_severe,
                    }),
                    ..Default::default()
                },
                |_ui| {},
            ) {
                self.invalidate_spc_fetch_request();
                self.save_overlay_defaults();
                ctx.request_repaint();
            }
            if open_severe {
                self.sidebar_tab = SidebarTab::Severe;
            }
        }
        // mPING REPORTS — live crowd reports from the public display feed.
        {
            let mping_success_age = self
                .mping_fetched_at
                .map(|at| compact_layer_success_age(at, Instant::now()));
            let report_count = self.mping_reports.len();
            let mping_status = mping_success_age
                .as_deref()
                .map(|age| format!("{report_count} rpt · {age}"))
                .or_else(|| (report_count > 0).then(|| format!("{report_count} rpt")));
            let mping_hover = format!(
                "Recent mPING public display reports: precipitation, hail, wind damage, flooding, visibility, and winter impacts\nData courtesy of NOAA NSSL / University of Oklahoma\n{}{}",
                mping_success_age
                    .as_deref()
                    .map(|age| format!("Last successful update: {age}"))
                    .unwrap_or_else(|| "No successful update yet".to_owned()),
                self.mping_last_error
                    .as_deref()
                    .map(|error| format!("\nLatest refresh failed: {error}"))
                    .unwrap_or_default()
            );
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.mping_enabled,
                        hover: "mPING crowd reports from the public display feed, refreshed about every 5 min",
                    },
                    name: "mPING",
                    name_hover: &mping_hover,
                    state: Some(if self.mping_rx.is_some() {
                        "loading"
                    } else if self.mping_last_error.is_some() {
                        "error"
                    } else if self.mping_fetched_at.is_some() {
                        "live"
                    } else {
                        "idle"
                    }),
                    count: mping_status.as_deref(),
                    ..Default::default()
                },
                |_ui| {},
            ) {
                self.mping_last_attempt_at = None;
                self.save_overlay_defaults();
                ctx.request_repaint();
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
            let warning_timeline = self.event_loop_hazard_window.is_some()
                || self.pending_event_loop_hazard_window.is_some();
            let warnings_success_age = warning_live_success_age(
                self.last_successful_live_hazard_refresh,
                warning_timeline,
                Instant::now(),
            );
            let warnings_count = warnings_success_age
                .as_deref()
                .map(|age| format!("{active_count} act · {age}"))
                .or_else(|| (active_count > 0).then(|| format!("{active_count} act")));
            let warning_freshness = if warning_timeline {
                "Archive/event warning window".to_owned()
            } else {
                warnings_success_age
                    .as_deref()
                    .map(|age| format!("Last successful live update: {age}"))
                    .unwrap_or_else(|| "No successful live update yet".to_owned())
            };
            let warnings_hover = format!(
                "Warning polygons (filters + full text in the Alerts tab)\n{}\n{}",
                if self.hazard_status.is_empty() {
                    "No warning load status"
                } else {
                    self.hazard_status.as_str()
                },
                warning_freshness
            );
            let mut open_severe = false;
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: &mut self.hazards_visible,
                        hover: "NWS warning/watch/MD polygons on the map",
                    },
                    name: "Warnings",
                    name_hover: &warnings_hover,
                    state: Some(if self.hazard_receiver.is_some() {
                        "loading"
                    } else if self.hazard_status.contains("failed")
                        || self.hazard_status.contains("disconnected")
                    {
                        "error"
                    } else if warning_timeline {
                        "archive"
                    } else if self.last_successful_live_hazard_refresh.is_some() {
                        "live"
                    } else {
                        "idle"
                    }),
                    count: warnings_count.as_deref(),
                    opacity: Some(LayerRowOpacity::U8 {
                        value: &mut fill_alpha,
                        min: 0,
                        max: 80,
                    }),
                    gear: Some(LayerRowGear::Open {
                        hover: "Open the Alerts tab (filters · fill · full text)",
                        clicked: &mut open_severe,
                    }),
                    ..Default::default()
                },
                |_ui| {},
            ) {
                self.app_settings.hazards_visible = self.hazards_visible;
                self.mark_app_settings_dirty();
                if fill_alpha != self.style_registry.hazard_global().fill_alpha {
                    self.set_all_hazard_fill_alpha(fill_alpha);
                    self.save_styles();
                }
                ctx.request_repaint();
            }
            if open_severe {
                self.sidebar_tab = SidebarTab::Severe;
            }
        }
        panel_kit::subgroup(ui, "Community", |ui| {
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
                if crate::placefiles::is_remote_source(&url) {
                    let show_text = self.style_registry.placefiles().default_show_text;
                    self.add_or_reload_placefile_source(url, show_text);
                    self.placefile_url_input.clear();
                    self.save_placefile_settings();
                    ctx.request_repaint();
                }
            }
        });
        if ui
            .button("Import downloaded placefile…")
            .on_hover_text(
                "Open one or more local GR-style placefiles. The picker shows every file, including extensionless and uncommon downloaded names. Local files remain in this layer list and can be refreshed or removed like URL feeds.",
            )
            .clicked()
            && let Some(paths) = rfd::FileDialog::new()
                .set_title("Import downloaded placefile")
                // Deliberately no extension filter: downloaded placefiles
                // commonly use .txt, .placefile, .pf, or no extension.
                .pick_files()
        {
            let show_text = self.style_registry.placefiles().default_show_text;
            for path in paths {
                self.add_or_reload_placefile_source(
                    crate::placefiles::persistent_local_source(&path),
                    show_text,
                );
            }
            self.save_placefile_settings();
            ctx.request_repaint();
        }
        let mut remove: Option<usize> = None;
        let mut changed = false;
        let mut placefiles_dirty = false;
        for (index, slot) in self.placefile_slots.iter_mut().enumerate() {
            let title = slot
                .data
                .as_ref()
                .map(|p| p.title.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| crate::placefiles::source_display_name(&slot.url));
            let placefile_success_age = slot
                .last_successful_load
                .map(|at| compact_layer_success_age(at, Instant::now()));
            let hover = format!(
                "{}\n{}\n{}",
                slot.url,
                slot.status,
                placefile_success_age
                    .as_deref()
                    .map(|age| format!("Last successful load: {age}"))
                    .unwrap_or_else(|| "No successful load yet".to_owned())
            );
            let compact_status = match (slot.data.as_ref(), placefile_success_age.as_deref()) {
                (Some(placefile), Some(age)) => {
                    format!("{} obj · {age}", placefile.objects.len())
                }
                (Some(placefile), None) => format!("{} obj", placefile.objects.len()),
                (None, _) => compact_layer_status(&slot.status, 22),
            };
            let state = if slot.receiver.is_some() {
                "loading"
            } else if slot.status.starts_with("load failed") {
                "error"
            } else if slot.data.is_some() {
                "live"
            } else {
                "queued"
            };
            // Field-split the slot so the row's vis toggle and the
            // trailing refresh button can borrow disjoint fields.
            let enabled = &mut slot.enabled;
            let next_refresh = &mut slot.next_refresh;
            let show_text = &mut slot.show_text;
            let visibility_range_percent = &mut slot.visibility_range_percent;
            let mut visibility_changed = false;
            let mut remove_this = false;
            if layer_row(
                ui,
                LayerRowSpec {
                    vis: LayerRowVis::Toggle {
                        value: enabled,
                        hover: "Show this placefile on the map",
                    },
                    name: &title,
                    name_hover: &hover,
                    state: Some(state),
                    count: Some(&compact_status),
                    gear: Some(LayerRowGear::Menu {
                        hover: "Placefile options",
                        content: Box::new(|ui| {
                            let (changed, _) =
                                placefile_visibility_range_menu(ui, visibility_range_percent);
                            visibility_changed |= changed;
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
            if visibility_changed {
                placefiles_dirty = true;
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
    /// composite suite. Default-closed; readiness hints and a direct Model
    /// action explain how to supply any missing prerequisites.
    pub(crate) fn oa_analysis_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // MESOANALYSIS: Bratseth obs-correction of the dock's
        // current surface field, stacked as its own "(OA)" layer.
        // Always render the section. A missing model field used to return
        // here, leaving a blank panel with no explanation or recovery path.
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
        ui.add_space(4.0);
        let oa_open = self.section_open("layers_analysis_oa", false);
        let oa_response = egui::CollapsingHeader::new("Analysis (OA)")
            .id_salt("oa_analysis_fold")
            .open(Some(oa_open))
            .show(ui, |ui| {
                    let var = oa_var.clone().unwrap_or_default();
                    ui.horizontal(|ui| {
                        let readiness = oa_analysis_readiness(
                            dock_has_field,
                            oa_var.is_some(),
                            self.obs_enabled,
                            !self.surface_obs.is_empty(),
                            self.model_lut.is_some(),
                            self.oa_rx.is_some(),
                        );
                        if ui
                            .add_enabled(
                                readiness == OaAnalysisReadiness::Ready,
                                egui::Button::new("Analyze obs"),
                            )
                            .on_hover_text(format!(
                                "Bratseth objective analysis: correct {var} with the live surface obs (converges to Optimal Interpolation; Bratseth 1986, ADAS weights, RTMA-style QC). Adds a \"{var} (OA)\" layer.",
                            ))
                            .clicked()
                        {
                            self.start_mesoanalysis(ctx);
                        }
                        match readiness {
                            OaAnalysisReadiness::NeedsModelField => {
                                ui.weak("Load a model field to begin analysis.");
                                if ui
                                    .small_button("Open Model")
                                    .on_hover_text("Open the Model window to download or select a field")
                                    .clicked()
                                {
                                    self.model_enabled = true;
                                    self.open_viewer(dock::WorkspacePane::Model);
                                    if self
                                        .model_dock
                                        .as_ref()
                                        .and_then(|dock| dock.newest_run())
                                        .is_none()
                                    {
                                        self.model_download_open = true;
                                    }
                                }
                            }
                            OaAnalysisReadiness::NeedsSupportedField => {
                                ui.weak("Show T2m, Td2m, or 10m wind to analyze.");
                            }
                            OaAnalysisReadiness::NeedsSurfaceObs => {
                                ui.weak("Turn on Surface obs above.");
                            }
                            OaAnalysisReadiness::WaitingForSurfaceObs => {
                                ui.weak("Waiting for the surface-observation fetch…");
                            }
                            OaAnalysisReadiness::NeedsMapLayer => {
                                ui.weak("Use \"Show on radar map\" in the Model window first.");
                            }
                            OaAnalysisReadiness::Busy => {
                                ui.spinner();
                                ui.weak("Analyzing observations…");
                            }
                            OaAnalysisReadiness::Ready => {}
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
                                "Nearest radiosonde launch site to the map center, at the launch nearest the displayed frame — 06/18z and other specials included (IEM archive, no key). Renders in the native skew-T with the full parameter suite. Tip: the RAOB stations layer puts every launch site on the map, click-to-sound.",
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
                            ui.menu_button("Composites ⏷", |ui| {
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
                            ui.menu_button("Derive (OA) ⏷", |ui| {
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
        panel_kit::row(ui, "Poll URL", |ui| {
            // Right-to-left: Start/Stop at the edge, the Feeds menu,
            // then the URL input truncating in whatever remains
            // (never forcing the panel wider — 320 pt rule).
            let label = if self.poll_active { "Stop" } else { "Start" };
            if ui.button(label).clicked() {
                if self.poll_active {
                    self.poll_active = false;
                    self.primary.live.dedupe_key = None;
                    self.poll_next = None;
                } else if normalized_poll_url(&self.poll_url).is_empty() {
                    self.status = "Poll URL: enter a URL or choose a saved link".to_owned();
                } else {
                    let url = self.poll_url.clone();
                    self.start_known_feed_poll(&url);
                }
            }
            ui.menu_button("Feeds ⏷", |ui| {
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
            ui.add(
                        egui::TextEdit::singleline(&mut self.poll_url)
                            .hint_text("http://host:port/path")
                            .desired_width(ui.available_width()),
                    )
                    .on_hover_text(
                        "GR2A-style polling: a served directory containing dir.list (the convention DOW/mobile radar crews use). Newest file loads automatically every 15 s, decoded natively (Level II or DORADE), and joins the frame loop.",
                    );
        });
        if self.poll_active && matches!(self.primary.feed, FeedSource::CustomUrl(_)) {
            panel_kit::status_block(
                ui,
                self.primary
                    .live
                    .dedupe_key
                    .as_deref()
                    .unwrap_or("waiting for dir.list…"),
                None,
            );
        }
        self.custom_poll_links_section(ui, ctx);
        self.intl_feeds_row(ui, ctx);
    }

    /// Start the shared poller on a known research-feed poll root — the
    /// Feeds-menu click path, reused verbatim by the community map
    /// markers so there is exactly one custom-URL start sequence.
    fn custom_poll_links_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(4.0);
        // Wrapped action rows: the three fill buttons fold under the label
        // at 320 pt.
        ui.horizontal_wrapped(|ui| {
            ui.label("Custom links").on_hover_text(
                "Saved GR2A-style poll roots for private/mobile radars. Each saved link can also carry a marker position, drawn as a red dot on the map.",
            );
            if ui
                .button("Use loaded")
                .on_hover_text("Fill label/site/lat/lon from the currently loaded radar volume")
                .clicked()
            {
                if let Some(volume) = &self.volume {
                    self.custom_poll_label_input = volume.site.name.clone().unwrap_or_else(|| {
                        if volume.site.id.is_empty() {
                            poll_url_name(&self.poll_url)
                        } else {
                            volume.site.id.clone()
                        }
                    });
                    self.custom_poll_site_input = volume.site.id.clone();
                    if let Some((lat, lon)) = self.loaded_volume_location() {
                        self.custom_poll_lat_input = format!("{lat:.5}");
                        self.custom_poll_lon_input = format!("{lon:.5}");
                    }
                } else {
                    self.status = "Custom poll link: no loaded radar to copy".to_owned();
                }
            }
            if ui
                .button("Map center")
                .on_hover_text("Fill lat/lon from the current map center")
                .clicked()
            {
                self.custom_poll_lat_input = format!("{:.5}", self.map_center_lat);
                self.custom_poll_lon_input = format!("{:.5}", self.map_center_lon);
            }
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            if ui
                .button("Import GIS…")
                .on_hover_text(
                    "Import GR customradars.gis/radars.gis rows. The current Poll URL is used as the base root; use {site} in the URL to control per-site expansion.",
                )
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("Import GR custom radar GIS")
                    .add_filter("GR radar GIS", &["gis", "txt"])
                    .pick_file()
            {
                match std::fs::read_to_string(&path) {
                    Ok(text) => self.import_custom_poll_gis_text(&text),
                    Err(err) => {
                        self.status = format!("Custom GIS import: {err}");
                    }
                }
                ctx.request_repaint();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.custom_poll_label_input)
                    .hint_text("label")
                    .desired_width(86.0),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.custom_poll_site_input)
                    .hint_text("site")
                    .desired_width(58.0),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.custom_poll_lat_input)
                    .hint_text("lat")
                    .desired_width(62.0),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.custom_poll_lon_input)
                    .hint_text("lon")
                    .desired_width(68.0),
            );
            if ui
                .button("Save")
                .on_hover_text(
                    "Save the current Poll URL. Leave lat/lon blank for a link-only entry; fill both to draw a red map marker.",
                )
                .clicked()
            {
                self.save_custom_poll_link_from_inputs();
                ctx.request_repaint();
            }
        });

        let mut start_index = None;
        let mut edit_index = None;
        let mut remove_index = None;
        if self.app_settings.custom_poll_links.is_empty() {
            ui.weak("No custom poll links saved yet.");
        } else {
            egui::ScrollArea::vertical()
                .id_salt("custom_poll_links")
                .max_height(118.0)
                .show(ui, |ui| {
                    for (index, entry) in self.app_settings.custom_poll_links.iter().enumerate() {
                        let label = custom_poll_entry_label(entry);
                        let site_id = entry.site_id.trim();
                        let coords = custom_poll_entry_lat_lon(entry)
                            .map(|(lat, lon)| format!(" {lat:.3}, {lon:.3}"))
                            .unwrap_or_else(|| " no marker".to_owned());
                        let active = self.poll_active
                            && matches!(self.primary.feed, FeedSource::CustomUrl(_))
                            && poll_urls_match(&self.poll_url, &entry.poll_url);
                        // Saved-link row grammar: dot + truncating title +
                        // right-aligned Poll/Edit/remove cluster (fits 320).
                        const LINK_CLUSTER_W: f32 = 110.0;
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 116, 118),
                                if active { "⏺" } else { "○" },
                            );
                            let title = if site_id.is_empty() {
                                label.clone()
                            } else {
                                format!("{site_id} {label}")
                            };
                            let title_width = (ui.available_width() - LINK_CLUSTER_W).max(60.0);
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(title_width, crate::PANEL_BUTTON_HEIGHT),
                                egui::Sense::hover(),
                            );
                            ui.put(rect, egui::Label::new(title).truncate())
                                .on_hover_text(format!("{}\n{}", entry.poll_url, coords));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .button("×")
                                        .on_hover_text("Remove this custom link")
                                        .clicked()
                                    {
                                        remove_index = Some(index);
                                    }
                                    if ui.button("Edit").clicked() {
                                        edit_index = Some(index);
                                    }
                                    if ui.button("Poll").clicked() {
                                        start_index = Some(index);
                                    }
                                },
                            );
                        });
                    }
                });
        }

        if let Some(index) = start_index {
            self.start_custom_poll_link(index);
        }
        if let Some(index) = edit_index
            && let Some(entry) = self.app_settings.custom_poll_links.get(index)
        {
            self.poll_url = entry.poll_url.clone();
            self.custom_poll_label_input = entry.label.clone();
            self.custom_poll_site_input = entry.site_id.clone();
            if let Some((lat, lon)) = custom_poll_entry_lat_lon(entry) {
                self.custom_poll_lat_input = format!("{lat:.5}");
                self.custom_poll_lon_input = format!("{lon:.5}");
            } else {
                self.custom_poll_lat_input.clear();
                self.custom_poll_lon_input.clear();
            }
        }
        if let Some(index) = remove_index
            && index < self.app_settings.custom_poll_links.len()
        {
            let label = custom_poll_entry_label(&self.app_settings.custom_poll_links[index]);
            self.app_settings.custom_poll_links.remove(index);
            let _ = self.app_settings.save();
            self.status = format!("Removed custom poll link {label}");
            ctx.request_repaint();
        }
    }

    fn save_custom_poll_link_from_inputs(&mut self) {
        let url = normalized_poll_url(&self.poll_url);
        if url.is_empty() {
            self.status = "Custom poll link: enter a Poll URL first".to_owned();
            return;
        }
        let (lat_e6, lon_e6) = match parse_custom_poll_marker_inputs(
            &self.custom_poll_lat_input,
            &self.custom_poll_lon_input,
        ) {
            Ok(coords) => coords,
            Err(message) => {
                self.status = message.to_owned();
                return;
            }
        };
        let site_id = self.custom_poll_site_input.trim().to_owned();
        let label = self.custom_poll_label_input.trim().to_owned();
        let entry = settings::CustomPollLinkEntry {
            label: if label.is_empty() {
                poll_url_name(&url)
            } else {
                label
            },
            site_id: site_id.clone(),
            lat_e6,
            lon_e6,
            poll_url: url.clone(),
        };
        let replace_index = self
            .app_settings
            .custom_poll_links
            .iter()
            .position(|existing| {
                poll_urls_match(&existing.poll_url, &url)
                    || (!site_id.is_empty()
                        && existing
                            .site_id
                            .trim()
                            .eq_ignore_ascii_case(site_id.as_str()))
            });
        let label = custom_poll_entry_label(&entry);
        if let Some(index) = replace_index {
            self.app_settings.custom_poll_links[index] = entry;
            self.status = format!("Updated custom poll link {label}");
        } else {
            self.app_settings.custom_poll_links.push(entry);
            self.status = format!("Saved custom poll link {label}");
        }
        self.poll_url = url;
        self.set_custom_url_poll_source();
        let _ = self.app_settings.save();
    }

    // Native-dialog import path; retained for unsupported non-desktop targets.
    #[cfg_attr(
        not(any(windows, target_os = "macos", target_os = "linux")),
        allow(dead_code)
    )]
    fn import_custom_poll_gis_text(&mut self, text: &str) {
        let base_url = self.poll_url.clone();
        let entries = match custom_poll_links_from_gis(text, &base_url) {
            Ok(entries) => entries,
            Err(message) => {
                self.status = format!("Custom GIS import: {message}");
                return;
            }
        };
        let imported = entries.len();
        let mut updated = 0usize;
        for entry in entries {
            if self.upsert_custom_poll_link(entry) {
                updated += 1;
            }
        }
        let added = imported.saturating_sub(updated);
        let _ = self.app_settings.save();
        self.status = format!("Custom GIS import: {added} added, {updated} updated");
    }

    // Native-dialog import path; retained for unsupported non-desktop targets.
    #[cfg_attr(
        not(any(windows, target_os = "macos", target_os = "linux")),
        allow(dead_code)
    )]
    fn upsert_custom_poll_link(&mut self, entry: settings::CustomPollLinkEntry) -> bool {
        let url = normalized_poll_url(&entry.poll_url);
        let site_id = entry.site_id.trim().to_owned();
        let replace_index = self
            .app_settings
            .custom_poll_links
            .iter()
            .position(|existing| {
                poll_urls_match(&existing.poll_url, &url)
                    || (!site_id.is_empty()
                        && existing
                            .site_id
                            .trim()
                            .eq_ignore_ascii_case(site_id.as_str()))
            });
        let mut entry = entry;
        entry.poll_url = url;
        if let Some(index) = replace_index {
            self.app_settings.custom_poll_links[index] = entry;
            true
        } else {
            self.app_settings.custom_poll_links.push(entry);
            false
        }
    }

    pub(crate) fn start_known_feed_poll(&mut self, url: &str) {
        let next_url = normalized_poll_url(url);
        let same_source = self.poll_active
            && matches!(self.primary.feed, FeedSource::CustomUrl(_))
            && poll_urls_match(&self.poll_url, &next_url);
        if !same_source {
            self.clear_frame_history();
            self.intl_loop_rx = None;
        }
        self.poll_url = next_url;
        self.set_custom_url_poll_source();
        self.poll_active = true;
        self.primary.live.dedupe_key = None;
        self.poll_next = None;
        self.poll_rx = None;
        // An auto-refresh load already in flight would land AFTER the
        // first poll install and wipe the polled frames — drop it.
        self.load_receiver = None;
        self.app_settings.poll_url = self.poll_url.clone();
        let _ = self.app_settings.save();
    }

    /// Point the shared poller at the URL text field (the custom-URL
    /// Start/known-feed click). Switching away from a different source
    /// drops a still-in-flight tick so it can't install under the new one.
    pub(crate) fn set_custom_url_poll_source(&mut self) {
        self.poll_url = normalized_poll_url(&self.poll_url);
        let source = FeedSource::CustomUrl(self.poll_url.clone());
        if self.primary.feed != source {
            self.poll_rx = None;
            self.primary.feed = source;
        }
    }

    /// INTERNATIONAL — national open-data radar feeds from data_source's
    /// provider registry (providers from other lanes appear here
    /// automatically once registered in `intl_providers()`). Picking a
    /// site starts the shared poller in Intl mode; Start resumes the
    /// persisted last selection, mirroring the poll URL row.
    fn coverage_badge(ui: &mut egui::Ui, label: &str, enabled: bool) {
        let color = if enabled {
            egui::Color32::from_rgb(108, 190, 132)
        } else {
            egui::Color32::from_rgb(118, 126, 138)
        };
        ui.colored_label(color, label);
    }

    pub(crate) fn radar_coverage_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let capabilities = data_source::international::intl_provider_capabilities();
        if !capabilities
            .iter()
            .any(|capability| capability.provider_id == self.coverage_provider_id)
            && let Some(first) = capabilities.first()
        {
            self.coverage_provider_id = first.provider_id.to_owned();
        }

        let provider_sites = data_source::international::intl_static_sites()
            .iter()
            .filter(|site| site.provider_id == self.coverage_provider_id)
            .collect::<Vec<_>>();
        if !provider_sites
            .iter()
            .any(|site| site.site_id == self.coverage_site_id)
            && let Some(first) = provider_sites.first()
        {
            self.coverage_site_id = first.site_id.clone();
        }

        let selected_capability = capabilities
            .iter()
            .find(|capability| capability.provider_id == self.coverage_provider_id);
        panel_kit::row(ui, "Provider", |ui| {
            let provider_text = selected_capability
                .map(|capability| capability.provider_label)
                .unwrap_or("Provider");
            let mut next_provider = self.coverage_provider_id.clone();
            egui::ComboBox::from_id_salt("coverage_provider_combo")
                .selected_text(provider_text)
                .width(ui.available_width().clamp(120.0, 200.0))
                .show_ui(ui, |ui| {
                    for capability in &capabilities {
                        ui.selectable_value(
                            &mut next_provider,
                            capability.provider_id.to_owned(),
                            format!("{} ({})", capability.provider_label, capability.country),
                        );
                    }
                })
                .response
                .on_hover_text(
                    "Choose a provider/site, probe the recent catalog without downloading volumes, then load the same path.",
                );
            if next_provider != self.coverage_provider_id {
                self.coverage_provider_id = next_provider;
                if let Some(first) = data_source::international::intl_static_sites()
                    .iter()
                    .find(|site| site.provider_id == self.coverage_provider_id)
                {
                    self.coverage_site_id = first.site_id.clone();
                } else {
                    self.coverage_site_id.clear();
                }
                self.coverage_probe_result = None;
            }
        });
        panel_kit::row(ui, "Site", |ui| {
            let site_text = provider_sites
                .iter()
                .find(|site| site.site_id == self.coverage_site_id)
                .map(|site| format!("{} {}", site.site_id, site.label))
                .unwrap_or_else(|| "Site".to_owned());
            let mut next_site = self.coverage_site_id.clone();
            egui::ComboBox::from_id_salt("coverage_site_combo")
                .selected_text(site_text)
                .width(ui.available_width().clamp(120.0, 240.0))
                .show_ui(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("coverage_site_combo_scroll")
                        .max_height(280.0)
                        .show(ui, |ui| {
                            for site in &provider_sites {
                                ui.selectable_value(
                                    &mut next_site,
                                    site.site_id.clone(),
                                    format!("{} - {}", site.site_id, site.label),
                                );
                            }
                        });
                });
            if next_site != self.coverage_site_id {
                self.coverage_site_id = next_site;
                self.coverage_probe_result = None;
            }
        });
        panel_kit::row(ui, "Frames", |ui| {
            if ui
                .add(
                    egui::DragValue::new(&mut self.coverage_frame_count)
                        .range(1..=crate::MAX_HISTORY_FRAME_LIMIT)
                        .speed(0.2),
                )
                .changed()
            {
                self.coverage_frame_count = self
                    .coverage_frame_count
                    .clamp(1, crate::MAX_HISTORY_FRAME_LIMIT);
            }
        });

        if let Some(capability) = selected_capability {
            ui.horizontal_wrapped(|ui| {
                Self::coverage_badge(ui, "Live", capability.live);
                Self::coverage_badge(ui, "Loop", capability.recent_loop);
                Self::coverage_badge(ui, "Archive", capability.archive_lookup);
                ui.weak(format!("{} sites", capability.visible_sites));
                ui.weak(capability.current_window)
                    .on_hover_text(capability.upstream_window);
            });
            ui.weak(capability.bowecho_status)
                .on_hover_text(format!("Next unlock: {}", capability.next_unlock));
        }

        let busy = self.coverage_probe_rx.in_flight();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new("Probe"))
                .on_hover_text("List recent frame plans only; no radar volumes are downloaded")
                .clicked()
            {
                self.start_coverage_probe(ctx);
            }
            if busy {
                ui.spinner();
            }
            if ui
                .button("Use live")
                .on_hover_text("Start live polling this provider/site")
                .clicked()
                && !self.coverage_provider_id.is_empty()
                && !self.coverage_site_id.is_empty()
            {
                self.start_intl_poll(
                    self.coverage_provider_id.clone(),
                    self.coverage_site_id.clone(),
                    ctx,
                );
            }
            if ui
                .add_enabled(
                    !busy && self.intl_loop_rx.is_none(),
                    egui::Button::new("Load loop"),
                )
                .on_hover_text(
                    "Start this site live, then load the selected number of recent frames",
                )
                .clicked()
                && !self.coverage_provider_id.is_empty()
                && !self.coverage_site_id.is_empty()
            {
                self.primary.limits.frame_limit = self
                    .primary
                    .limits
                    .frame_limit
                    .max(self.coverage_frame_count)
                    .min(crate::MAX_HISTORY_FRAME_LIMIT);
                self.start_intl_poll(
                    self.coverage_provider_id.clone(),
                    self.coverage_site_id.clone(),
                    ctx,
                );
                self.start_intl_loop_load(ctx);
            }
            if !self.coverage_provider_id.is_empty() && !self.coverage_site_id.is_empty() {
                // The one archive gate (spec §1.3): the button and its
                // greyed hover reason derive from the same call that
                // powers the browser it opens.
                let site = data_source::sites::SiteRef::Intl {
                    provider_id: self.coverage_provider_id.clone(),
                    site_id: self.coverage_site_id.clone(),
                };
                match crate::archive_browser::archive_access(&site) {
                    crate::archive_browser::ArchiveAccess::None { reason } => {
                        ui.add_enabled(false, egui::Button::new("Browse archive"))
                            .on_disabled_hover_text(reason);
                    }
                    _ => {
                        if ui
                            .button("Browse archive")
                            .on_hover_text(
                                "Make this site the primary display owner and list its \
                                 provider archive in Data \u{25b8} Archive",
                            )
                            .clicked()
                        {
                            self.open_intl_archive_browser(
                                self.coverage_provider_id.clone(),
                                self.coverage_site_id.clone(),
                                ctx,
                            );
                        }
                    }
                }
            }
        });

        match &self.coverage_probe_result {
            Some(Ok(probe)) => {
                let current = probe.provider_id == self.coverage_provider_id
                    && probe.site_id == self.coverage_site_id;
                let prefix = if current { "Probe" } else { "Last probe" };
                ui.weak(format!(
                    "{prefix}: {}/{} frames in {} ms at {}",
                    probe.frame_count,
                    probe.requested,
                    probe.elapsed_ms,
                    probe.checked_at_utc.format("%H:%M:%SZ")
                ));
                if let Some(first) = &probe.first_identity {
                    ui.weak(format!(
                        "First {} ({} part{})",
                        crate::compact_intl_identity(first),
                        probe.first_part_count,
                        if probe.first_part_count == 1 { "" } else { "s" }
                    ));
                }
                if let Some(latest) = &probe.latest_identity {
                    ui.weak(format!(
                        "Latest {} ({} part{})",
                        crate::compact_intl_identity(latest),
                        probe.latest_part_count,
                        if probe.latest_part_count == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ));
                }
            }
            Some(Err(message)) => {
                ui.colored_label(
                    egui::Color32::from_rgb(255, 130, 130),
                    format!("Probe failed: {message}"),
                );
            }
            None => {}
        }

        egui::CollapsingHeader::new("Provider capability rows")
            .id_salt("coverage_provider_rows")
            .default_open(false)
            .show(ui, |ui| {
                for capability in capabilities {
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(capability.provider_label);
                        ui.weak(format!("{} sites", capability.visible_sites));
                        Self::coverage_badge(ui, "Loop", capability.recent_loop);
                        Self::coverage_badge(ui, "Archive", capability.archive_lookup);
                    });
                    ui.weak(format!(
                        "{} - upstream: {}",
                        capability.current_window, capability.upstream_window
                    ));
                    ui.weak(format!(
                        "{}; next: {}",
                        capability.bowecho_status, capability.next_unlock
                    ));
                    ui.add_space(4.0);
                }
            });
    }

    fn intl_feeds_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut list_provider: Option<String> = None;
        let mut start: Option<(String, String)> = None;
        let intl_polling =
            self.poll_active && matches!(self.primary.feed, FeedSource::Live(SiteRef::Intl { .. }));
        panel_kit::row(ui, "International", |ui| {
            // Right-to-left: Start/Stop at the edge, Site, then Country.
            if intl_polling {
                if ui.button("Stop").clicked() {
                    self.poll_active = false;
                    self.primary.live.dedupe_key = None;
                    self.poll_next = None;
                }
            } else if let Some(FeedSource::Live(SiteRef::Intl {
                provider_id,
                site_id,
            })) = FeedSource::intl_from_settings(&self.app_settings)
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
            if let Some(sites) = &self.intl_sites {
                ui.menu_button("Site ⏷", |ui| {
                    ui.set_min_width(160.0);
                    egui::ScrollArea::vertical()
                        .id_salt("intl_site_list")
                        .max_height(300.0)
                        .show(ui, |ui| {
                            for site in sites {
                                if ui.button(&site.label).clicked() {
                                    start =
                                        Some((site.provider_id.to_owned(), site.site_id.clone()));
                                    ui.close();
                                }
                            }
                        });
                });
            }
            if self.intl_sites_rx.in_flight() {
                ui.spinner();
            }
            let provider_button = if self.intl_picker_provider.is_empty() {
                "Country ⏷".to_owned()
            } else {
                format!("{} ⏷", intl_provider_label(&self.intl_picker_provider))
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
            .on_hover_text(
                "National open-data radar networks (ODIM_H5 polar volumes — OPERA Data Information Model), decoded natively. Pick a country's provider, then a site: the newest volume polls every 60 s and joins the frame loop like any polled feed. Providers grouped by country.",
            );
        });
        if intl_polling {
            // Same status grammar as the URL poll: the dedupe key of the
            // installed frame (Polled:/Poll: live in self.status).
            panel_kit::status_block(
                ui,
                self.primary
                    .live
                    .dedupe_key
                    .as_deref()
                    .unwrap_or("waiting for catalog…"),
                None,
            );
        }
        if let Some(provider_id) = list_provider {
            self.start_intl_site_listing(&provider_id, ctx);
        }
        if let Some((provider_id, site_id)) = start {
            self.start_intl_poll(provider_id, site_id, ctx);
        }
    }

    pub(crate) fn add_layer_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // THE single front door for every map data type (proposal
        // section 4 step 5 / discoverability fix 1.4): you no longer
        // need to know that layers are born inside the Model/Sat
        // windows' "Show on radar map" buttons.
        ui.add_space(4.0);
        let mut add_site: Option<RadarSite> = None;
        let mut add_intl_site: Option<data_source::international::IntlSite> = None;
        let mut add_italy_dpc: Option<ItalyDpcMapProduct> = None;
        let mut add_taiwan_cwa = false;
        let mut add_grid_composite: Option<grid_composites::GridCompositeSource> = None;
        ui.menu_button("+ Add layer ⏷", |ui| {
                    ui.menu_button("Radar overlay", |ui| {
                        ui.set_min_width(220.0);
                        egui::ScrollArea::vertical()
                            .id_salt("add_layer_site_list")
                            .max_height(300.0)
                            .show(ui, |ui| {
                                // Favorites first (spec §2.4) — BOTH
                                // worlds (v0.29 Phase 3: `intl:` keys
                                // resolve to intl overlay feeds) — then
                                // the full alphabetical US list.
                                let favorites: Vec<&RadarSite> = self
                                    .sites
                                    .iter()
                                    .filter(|site| {
                                        self.app_settings.is_favorite(&site.level2_id)
                                    })
                                    .collect();
                                let intl_favorites: Vec<
                                    data_source::international::IntlSite,
                                > = self
                                    .app_settings
                                    .favorites
                                    .iter()
                                    .filter_map(|fav| {
                                        match data_source::sites::SiteRef::parse_settings_key(fav) {
                                            data_source::sites::SiteRef::Intl {
                                                provider_id,
                                                site_id,
                                            } => Self::find_intl_site(&provider_id, &site_id),
                                            data_source::sites::SiteRef::Us { .. } => None,
                                        }
                                    })
                                    .collect();
                                if !favorites.is_empty() || !intl_favorites.is_empty() {
                                    for site in favorites {
                                        if ui
                                            .button(format!("★ {}", format_site_label(site)))
                                            .clicked()
                                        {
                                            add_site = Some(site.clone());
                                            ui.close();
                                        }
                                    }
                                    for site in intl_favorites {
                                        if ui
                                            .button(format!("★ {}", site.label))
                                            .clicked()
                                        {
                                            add_intl_site = Some(site);
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
                        "Another radar drawn over the map (tip: right-click the map > \"lowest beam here\" does this too)",
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
                    ui.menu_button("Grid / Composites", |ui| {
                        ui.menu_button("Italy DPC radar", |ui| {
                            ui.set_min_width(210.0);
                            for product in ItalyDpcMapProduct::ALL {
                                if ui.button(product.label()).clicked() {
                                    add_italy_dpc = Some(product);
                                    ui.close();
                                }
                            }
                        })
                        .response
                        .on_hover_text(
                            "Official Protezione Civile / Radar-DPC national gridded radar products",
                        );
                        if ui.button("Taiwan CWA composite reflectivity").clicked() {
                            add_taiwan_cwa = true;
                            ui.close();
                        }
                        ui.menu_button("Poland IMGW dual-pol CMAX", |ui| {
                            ui.set_min_width(245.0);
                            for &site in IMGW_POLRAD_SITES {
                                ui.menu_button(
                                    format!("{} ({})", site.label(), site.system_code()),
                                    |ui| {
                                        for &quantity in
                                            grid_composites::imgw_quantities_for_site(site)
                                        {
                                            if ui.button(quantity.label()).clicked() {
                                                add_grid_composite = Some(
                                                    grid_composites::GridCompositeSource::imgw_polrad(
                                                        site, quantity,
                                                    ),
                                                );
                                                ui.close();
                                            }
                                        }
                                    },
                                );
                            }
                        })
                        .response
                        .on_hover_text(format!(
                            "Official IMGW-PIB POLRAD 2-D site-centered ODIM HDF5 maximum products (not polar volumes; quantity availability varies by radar).\n\n{}\n{}",
                            data_source::grid_products::imgw::IMGW_SOURCE_NOTICE_PL,
                            data_source::grid_products::imgw::IMGW_PROCESSED_NOTICE_PL,
                        ));
                        ui.menu_button("MRMS", |ui| {
                            if ui.button("Lowest-altitude reflectivity").clicked() {
                                add_grid_composite = Some(
                                    grid_composites::GridCompositeSource::MrmsLowestAltitudeReflectivity,
                                );
                                ui.close();
                            }
                            if ui.button("Composite reflectivity").clicked() {
                                add_grid_composite = Some(
                                    grid_composites::GridCompositeSource::MrmsCompositeReflectivity,
                                );
                                ui.close();
                            }
                        })
                        .response
                        .on_hover_text("NOAA/NCEP MRMS latest public GRIB2 grids");
                        if ui.button("EUMETNET OPERA DBZH composite").clicked() {
                            add_grid_composite =
                                Some(grid_composites::GridCompositeSource::EumetnetOperaDbzh);
                            ui.close();
                        }
                    })
                    .response
                    .on_hover_text("National and regional gridded/composite radar layers");
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
                        .button("SpotterNetwork reports")
                        .on_hover_text(
                            "Add the public SpotterNetwork reports-only placefile (recent report icons, 1-min refresh)",
                        )
                        .clicked()
                    {
                        let url = "https://www.spotternetwork.org/feeds/reports.txt".to_owned();
                        if !self.placefile_slots.iter().any(|slot| slot.url == url) {
                            let mut slot = PlacefileSlot::new(url, true);
                            slot.show_text = false; // report details are in the hover card
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
                        .button("Satellite…")
                        .on_hover_text(
                            "Open the GOES, Himawari, Meteosat, and saved-loop Satellite window",
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
                });
        if let Some(site) = add_site {
            self.add_or_refresh_radar_layer(site, ctx);
        }
        if let Some(site) = add_intl_site {
            self.add_or_refresh_intl_radar_layer(&site, ctx);
        }
        if let Some(product) = add_italy_dpc {
            self.add_italy_dpc_layer(product, ctx);
        }
        if add_taiwan_cwa {
            self.add_taiwan_cwa_layer(ctx);
        }
        if let Some(source) = add_grid_composite {
            self.add_grid_composite_layer(source, ctx);
        }
        // A composite picked from the menu becomes a layer immediately, even
        // while the Analysis (OA) section is collapsed (its own consumer
        // only runs when its fold is open).
        if let Some(pick) = self.take_composite_pick() {
            self.push_composite_layer(pick, ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oa_analysis_readiness_explains_every_blocked_state() {
        use OaAnalysisReadiness::*;

        assert_eq!(
            oa_analysis_readiness(false, false, false, false, false, false),
            NeedsModelField
        );
        assert_eq!(
            oa_analysis_readiness(true, false, true, true, true, false),
            NeedsSupportedField
        );
        assert_eq!(
            oa_analysis_readiness(true, true, false, false, false, false),
            NeedsSurfaceObs
        );
        assert_eq!(
            oa_analysis_readiness(true, true, true, false, false, false),
            WaitingForSurfaceObs
        );
        assert_eq!(
            oa_analysis_readiness(true, true, true, true, false, false),
            NeedsMapLayer
        );
        assert_eq!(
            oa_analysis_readiness(true, true, true, true, true, true),
            Busy
        );
        assert_eq!(
            oa_analysis_readiness(true, true, true, true, true, false),
            Ready
        );
    }

    #[test]
    fn missing_model_field_is_the_primary_analysis_recovery_state() {
        assert_eq!(
            oa_analysis_readiness(false, true, true, true, true, true),
            OaAnalysisReadiness::NeedsModelField
        );
    }

    #[test]
    fn grid_composite_hover_does_not_claim_model_sounding_behavior() {
        let visibility = model_map_layer_visibility_hover(true);
        assert!(visibility.contains("gridded radar/composite"));
        assert!(!visibility.contains("soundings"));
        assert_eq!(
            model_map_layer_opacity_hover(true),
            "Gridded radar/composite layer opacity"
        );
    }

    #[test]
    fn placefile_visibility_range_is_selectable_inside_gear_menu() {
        #[derive(Default)]
        struct FrameState {
            gear_rect: Option<egui::Rect>,
            second_choice_rect: Option<egui::Rect>,
            menu_rendered: bool,
        }

        fn raw_input(events: Vec<egui::Event>) -> egui::RawInput {
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                ..Default::default()
            }
        }

        fn pointer_input(position: egui::Pos2, pressed: bool) -> egui::RawInput {
            raw_input(vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                },
            ])
        }

        fn frame(ctx: &egui::Context, input: egui::RawInput, selection: &mut u16) -> FrameState {
            let mut state = FrameState::default();
            let _ = ctx.run_ui(input, |ui| {
                let menu = ui.menu_button("⚙", |ui| {
                    state.menu_rendered = true;
                    let (_, choices) = placefile_visibility_range_menu(ui, selection);
                    state.second_choice_rect = choices
                        .into_iter()
                        .find_map(|(percent, response)| (percent == 200).then_some(response.rect));
                });
                state.gear_rect = Some(menu.response.rect);
            });
            state
        }

        fn click(ctx: &egui::Context, position: egui::Pos2, selection: &mut u16) -> FrameState {
            let _ = frame(ctx, pointer_input(position, true), selection);
            frame(ctx, pointer_input(position, false), selection)
        }

        let ctx = egui::Context::default();
        let mut selection = 100;

        let initial = frame(&ctx, raw_input(Vec::new()), &mut selection);
        assert!(!initial.menu_rendered);
        let gear_center = initial
            .gear_rect
            .expect("gear button must be laid out")
            .center();

        let opened = click(&ctx, gear_center, &mut selection);
        assert!(opened.menu_rendered, "gear click must open the range menu");
        let settled = frame(&ctx, raw_input(Vec::new()), &mut selection);
        assert!(
            settled.menu_rendered,
            "range choices must remain visible after the menu opens"
        );
        let second_choice_center = settled
            .second_choice_rect
            .expect("2x range choice must be laid out directly in the menu")
            .center();

        let selected = click(&ctx, second_choice_center, &mut selection);
        assert_eq!(selection, 200, "the direct range choice must be clickable");
        assert!(
            selected.menu_rendered,
            "selecting a range must not make the placefile options disappear"
        );
    }
}
