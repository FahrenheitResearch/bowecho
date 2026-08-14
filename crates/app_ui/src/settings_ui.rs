//! Settings-window panels (the sidebar Layers/Data/Settings tabs) moved
//! verbatim out of `main.rs` (v0.29.4 decomposition, queue item #7).
//! Sidebar orchestration (`side_panel`, `sidebar_tab_bar`), the shared
//! section helpers (`remembered_section`, `section_open`, `section_header`,
//! `section_rule`), and settings loaders/appliers stay in `main.rs`; this
//! module reaches them via `crate::`.

use crate::*;

impl ViewerApp {
    pub(crate) fn customization_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.open_color_tables_request {
            self.open_color_tables_request = false;
            self.set_section_open("customize_appearance", true);
        }
        self.remembered_section(ui, "customize_map_layers", "Map layers", true, |app, ui| {
            app.add_layer_menu(ui, ctx);
            ui.separator();
            app.radar_marker_label_toggle(ui, ctx);
            ui.separator();
            app.layers_rail(ui, ctx);
        });
        self.remembered_section(
            ui,
            "customize_analysis_overlays",
            "Analysis overlays",
            false,
            |app, ui| {
                app.oa_analysis_section(ui, ctx);
            },
        );
        self.remembered_section(
            ui,
            "customize_appearance",
            "Appearance",
            false,
            |app, ui| {
                app.appearance_panel(ui, ctx);
            },
        );
    }

    fn appearance_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.appearance_profile_row(ui, ctx);
        ui.separator();
        panel_kit::row(ui, "Style overrides", |ui| {
            let reset_enabled =
                !self.styles_newer_schema && self.style_settings != Default::default();
            if ui
                .add_enabled(reset_enabled, egui::Button::new("Reset all"))
                .on_hover_text(if self.styles_newer_schema {
                    "styles.json was written by a newer BowEcho; reset is disabled so this build cannot clobber it"
                } else {
                    "Delete all styles.json overrides and return map appearance to built-in defaults"
                })
                .clicked()
            {
                self.reset_style_overrides(ctx);
            }
        });
        ui.separator();
        panel_kit::row(ui, "Map backdrop", |ui| {
            // Right-to-left control cluster: Reset sits at the row edge,
            // the color swatch to its left.
            if fixed_action_button(ui, "Reset", 52.0)
                .on_hover_text("Reset to the built-in dark map backdrop")
                .clicked()
            {
                self.style_settings.map.background_color = None;
                self.rebuild_style_registry();
                self.save_styles();
                ctx.request_repaint();
            }
            let mut color = self
                .style_settings
                .map
                .background_color
                .unwrap_or(self.style_registry.map().background_color);
            let response = ui.color_edit_button_srgba_unmultiplied(&mut color);
            if response.changed() {
                color[3] = 255;
                self.style_settings.map.background_color = Some(color);
                self.rebuild_style_registry();
                self.save_styles();
                ctx.request_repaint();
            }
        });
        ui.separator();
        self.hazard_polygon_style_panel(ui, ctx);
        ui.separator();
        self.radar_age_style_panel(ui, ctx);
        ui.separator();
        ui.label(egui::RichText::new("Color tables").strong());
        ui.weak("Active radar product");
        let active_product = self.selected_product.clone();
        self.active_product_color_picker(ui, ctx, &active_product);
        ui.separator();
        self.color_table_panel(ui, ctx);
    }

    fn radar_marker_label_toggle(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut show = self.app_settings.show_radar_labels;
        if ui
            .checkbox(&mut show, "Radar labels")
            .on_hover_text("Show radar-site and TDWR/Txxx labels next to map markers. Markers remain visible and clickable when this is off.")
            .changed()
        {
            self.app_settings.show_radar_labels = show;
            self.mark_app_settings_dirty();
            ctx.request_repaint();
        }
        ui.add_enabled_ui(self.app_settings.show_radar_labels, |ui| {
            panel_kit::row(ui, "Label style", |ui| {
                let mut style = RadarLabelStyle::from_key(&self.app_settings.radar_label_style);
                egui::ComboBox::from_id_salt("radar_label_style")
                    .selected_text(style.label())
                    .width(96.0)
                    .show_ui(ui, |ui| {
                        for option in RadarLabelStyle::ALL {
                            ui.selectable_value(&mut style, option, option.label());
                        }
                    });
                if style.key() != self.app_settings.radar_label_style {
                    self.app_settings.radar_label_style = style.key().to_owned();
                    self.mark_app_settings_dirty();
                    ctx.request_repaint();
                }
            });
        });
    }

    fn appearance_profile_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Profile");
            let active = self.active_appearance_profile();
            let modified = self.appearance_profile_modified(active);
            let selected_text = if modified {
                format!("{} (modified)", active.label())
            } else {
                active.label().to_owned()
            };
            let mut chosen = active;
            ui.add_enabled_ui(!self.styles_newer_schema, |ui| {
                egui::ComboBox::from_id_salt("appearance_profile")
                    .selected_text(selected_text)
                    .width(ui_theme::COMBO_MAX_W)
                    .show_ui(ui, |ui| {
                        for profile in AppearanceProfile::ALL {
                            ui.selectable_value(&mut chosen, profile, profile.label())
                                .on_hover_text(profile.description());
                        }
                    });
            })
            .response
            .on_disabled_hover_text(
                "styles.json was written by a newer BowEcho; profile switching is disabled so this build cannot clobber it",
            );

            if chosen != active && !self.styles_newer_schema {
                self.apply_appearance_profile(chosen, ctx);
            }
            if modified {
                ui.weak("modified");
            }
        });
    }

    fn radar_age_style_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(egui::RichText::new("Radar age").strong());
        let resolved = self.style_registry.radar_age().clone();
        // The two self-labeled toggles + the section Reset wrap as one
        // family; the value/color pairs below are kit rows.
        ui.horizontal_wrapped(|ui| {
            let mut ring_enabled = resolved.ring_enabled;
            if ui
                .checkbox(&mut ring_enabled, "Age ring")
                .on_hover_text("Color the radar data-edge ring by scan age")
                .changed()
            {
                self.style_settings.radar_age.ring_enabled = Some(ring_enabled);
                self.rebuild_style_registry();
                self.save_styles();
                ctx.request_repaint();
            }

            let mut glyph_arc_enabled = resolved.glyph_arc_enabled;
            if ui
                .checkbox(&mut glyph_arc_enabled, "Marker arc")
                .on_hover_text("Draw a fixed-size age arc around loaded radar site markers")
                .changed()
            {
                self.style_settings.radar_age.glyph_arc_enabled = Some(glyph_arc_enabled);
                self.rebuild_style_registry();
                self.save_styles();
                ctx.request_repaint();
            }

            if fixed_action_button(ui, "Reset", 52.0)
                .on_hover_text("Reset radar-age ring, marker arc, thresholds, chip, and colors")
                .clicked()
            {
                self.style_settings.radar_age = styles::RadarAgeStyleOverride::default();
                self.rebuild_style_registry();
                self.save_styles();
                ctx.request_repaint();
            }
        });

        let mut glyph_arc_radius_px = resolved.glyph_arc_radius_px.clamp(4.0, 24.0);
        let arc_radius_response = panel_kit::row(ui, "Arc radius", |ui| {
            ui.add(
                egui::DragValue::new(&mut glyph_arc_radius_px)
                    .range(4.0..=24.0)
                    .speed(0.25)
                    .suffix(" px"),
            )
            .on_hover_text("Fixed screen radius for loaded-site age arcs")
        });
        if arc_radius_response.changed() {
            self.style_settings.radar_age.glyph_arc_radius_px =
                Some(glyph_arc_radius_px.clamp(4.0, 24.0));
            self.rebuild_style_registry();
            ctx.request_repaint();
        }
        if arc_radius_response.drag_stopped()
            || (arc_radius_response.changed() && !arc_radius_response.dragged())
        {
            self.save_styles();
        }

        let mut stale_chip_minutes =
            (resolved.stale_chip_seconds.max(0) as f32 / 60.0).clamp(1.0, 60.0);
        let stale_chip_response = panel_kit::row(ui, "STALE chip after", |ui| {
            ui.add(
                egui::DragValue::new(&mut stale_chip_minutes)
                    .range(1.0..=60.0)
                    .speed(0.25)
                    .suffix(" min"),
            )
            .on_hover_text("Live radar older than this shows the STALE chip")
        });
        if stale_chip_response.changed() {
            self.style_settings.radar_age.stale_chip_seconds =
                Some((stale_chip_minutes * 60.0).round() as i64);
            self.rebuild_style_registry();
            ctx.request_repaint();
        }
        if stale_chip_response.drag_stopped()
            || (stale_chip_response.changed() && !stale_chip_response.dragged())
        {
            self.save_styles();
        }

        // Age thresholds: colors move fresh → aging → stale → expired.
        let mut fresh_minutes = (resolved.green_seconds.max(1) as f32 / 60.0).clamp(0.5, 240.0);
        let mut aging_minutes = (resolved.yellow_seconds.max(1) as f32 / 60.0).clamp(0.5, 240.0);
        let mut stale_minutes = (resolved.red_seconds.max(1) as f32 / 60.0).clamp(0.5, 240.0);
        let fresh_response = panel_kit::row(ui, "Fresh up to", |ui| {
            ui.add(
                egui::DragValue::new(&mut fresh_minutes)
                    .range(0.5..=240.0)
                    .speed(0.25)
                    .suffix(" min"),
            )
            .on_hover_text("Age that still renders as the fresh color")
        });
        let aging_response = panel_kit::row(ui, "Aging up to", |ui| {
            ui.add(
                egui::DragValue::new(&mut aging_minutes)
                    .range(0.5..=240.0)
                    .speed(0.25)
                    .suffix(" min"),
            )
            .on_hover_text("Age where the gradient reaches the aging color")
        });
        let stale_response = panel_kit::row(ui, "Stale up to", |ui| {
            ui.add(
                egui::DragValue::new(&mut stale_minutes)
                    .range(0.5..=240.0)
                    .speed(0.25)
                    .suffix(" min"),
            )
            .on_hover_text("Age where the gradient reaches stale and the marker arc becomes full")
        });
        if fresh_response.changed() || aging_response.changed() || stale_response.changed() {
            let fresh_seconds = ((fresh_minutes * 60.0).round() as i64).max(1);
            let aging_seconds = ((aging_minutes * 60.0).round() as i64).max(fresh_seconds);
            let stale_seconds = ((stale_minutes * 60.0).round() as i64).max(aging_seconds);
            self.style_settings.radar_age.green_seconds = Some(fresh_seconds);
            self.style_settings.radar_age.yellow_seconds = Some(aging_seconds);
            self.style_settings.radar_age.red_seconds = Some(stale_seconds);
            self.rebuild_style_registry();
            ctx.request_repaint();
        }
        let thresholds_committed = fresh_response.drag_stopped()
            || aging_response.drag_stopped()
            || stale_response.drag_stopped()
            || (fresh_response.changed() && !fresh_response.dragged())
            || (aging_response.changed() && !aging_response.dragged())
            || (stale_response.changed() && !stale_response.dragged());
        if thresholds_committed {
            self.save_styles();
        }

        // Age colors: one kit row per stop, swatch right-aligned.
        let mut age_colors_changed = false;
        for (label, stored, resolved_color) in [
            (
                "Fresh color",
                &mut self.style_settings.radar_age.fresh_color,
                resolved.fresh_color,
            ),
            (
                "Aging color",
                &mut self.style_settings.radar_age.aging_color,
                resolved.aging_color,
            ),
            (
                "Stale color",
                &mut self.style_settings.radar_age.stale_color,
                resolved.stale_color,
            ),
            (
                "Expired color",
                &mut self.style_settings.radar_age.expired_color,
                resolved.expired_color,
            ),
        ] {
            let mut color = stored.unwrap_or(resolved_color);
            let changed = panel_kit::row(ui, label, |ui| {
                ui.color_edit_button_srgba_unmultiplied(&mut color)
                    .changed()
            });
            if changed {
                *stored = Some(color);
                age_colors_changed = true;
            }
        }
        if age_colors_changed {
            self.rebuild_style_registry();
            self.save_styles();
            ctx.request_repaint();
        }
    }

    fn hazard_polygon_style_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(egui::RichText::new("Warning polygons").strong());
        let global = self.style_registry.hazard_global().clone();

        let mut fill_alpha = global.fill_alpha as f32;
        let fill_response =
            panel_kit::slider_row(ui, "All fills", &mut fill_alpha, 0.0..=80.0, 0.0, |value| {
                format!("{value:.0}")
            })
            .on_hover_text("Set warning-polygon fill opacity for every family (0 disables fills)");
        if fill_response.changed() {
            self.set_all_hazard_fill_alpha(fill_alpha.round() as u8);
            ctx.request_repaint();
        }
        if fill_response.drag_stopped() || (fill_response.changed() && !fill_response.dragged()) {
            self.save_styles();
        }

        let mut stroke_scale = global.stroke_width_scale;
        let stroke_response =
            panel_kit::slider_row(ui, "Width", &mut stroke_scale, 0.5..=3.0, 0.0, |value| {
                format!("{value:.2}x")
            })
            .on_hover_text("Warning-polygon outline width scale");
        if stroke_response.changed() {
            self.style_settings.hazard_global.stroke_width_scale =
                Some((stroke_scale * 100.0).round() / 100.0);
            self.rebuild_style_registry();
            ctx.request_repaint();
        }
        if stroke_response.drag_stopped()
            || (stroke_response.changed() && !stroke_response.dragged())
        {
            self.save_styles();
        }

        let mut selected_boost = global.selected_width_boost;
        let selected_response = panel_kit::slider_row(
            ui,
            "Selected +",
            &mut selected_boost,
            0.0..=4.0,
            0.0,
            |value| format!("{value:.2}"),
        )
        .on_hover_text("Extra outline width for the selected polygon");
        if selected_response.changed() {
            self.style_settings.hazard_global.selected_width_boost =
                Some((selected_boost * 100.0).round() / 100.0);
            self.rebuild_style_registry();
            ctx.request_repaint();
        }
        if selected_response.drag_stopped()
            || (selected_response.changed() && !selected_response.dragged())
        {
            self.save_styles();
        }

        if fixed_action_button(ui, "Reset", 52.0)
            .on_hover_text("Reset global warning-polygon alpha and width defaults")
            .clicked()
        {
            self.style_settings.hazard_global = styles::HazardGlobalOverride::default();
            self.rebuild_style_registry();
            self.save_styles();
            ctx.request_repaint();
        }

        let selection_id = ui.make_persistent_id("appearance_hazard_polygon_style_key");
        let mut selected_key = ctx
            .data_mut(|data| data.get_persisted::<String>(selection_id))
            .filter(|key| hazard_style_key_known(key))
            .unwrap_or_else(|| HAZARD_STYLE_DEFAULT_KEY.to_owned());
        panel_kit::row(ui, "Family", |ui| {
            // Right-to-left: Reset (when an override exists) at the edge,
            // the family combo to its left.
            if self.style_settings.hazards.contains_key(&selected_key)
                && fixed_action_button(ui, "Reset", 52.0)
                    .on_hover_text("Reset this warning polygon family to built-in styling")
                    .clicked()
            {
                self.style_settings.hazards.remove(&selected_key);
                self.rebuild_style_registry();
                self.save_styles();
                ctx.request_repaint();
            }
            egui::ComboBox::from_id_salt("appearance_hazard_polygon_style_combo")
                .selected_text(hazard_style_label(&selected_key))
                .width(ui.available_width().clamp(96.0, ui_theme::COMBO_MAX_W))
                .show_ui(ui, |ui| {
                    for key in hazard_style_keys() {
                        ui.selectable_value(
                            &mut selected_key,
                            (*key).to_owned(),
                            hazard_style_label(key),
                        );
                    }
                });
        });
        ctx.data_mut(|data| data.insert_persisted(selection_id, selected_key.clone()));

        let resolved = hazard_style_resolved_polygon(&self.style_registry, &selected_key);
        let existing = self
            .style_settings
            .hazards
            .get(&selected_key)
            .cloned()
            .unwrap_or_default();

        let mut stroke_color = existing.stroke_color.unwrap_or(resolved.stroke_color);
        if panel_kit::row(ui, "Stroke color", |ui| {
            ui.color_edit_button_srgba_unmultiplied(&mut stroke_color)
                .changed()
        }) {
            self.style_settings
                .hazards
                .entry(selected_key.clone())
                .or_default()
                .stroke_color = Some(stroke_color);
            self.rebuild_style_registry();
            self.save_styles();
            ctx.request_repaint();
        }

        let mut fill_color = existing.fill_color.unwrap_or(resolved.fill_color);
        if panel_kit::row(ui, "Fill color", |ui| {
            ui.color_edit_button_srgba_unmultiplied(&mut fill_color)
                .changed()
        }) {
            self.style_settings
                .hazards
                .entry(selected_key.clone())
                .or_default()
                .fill_color = Some(fill_color);
            self.rebuild_style_registry();
            self.save_styles();
            ctx.request_repaint();
        }

        let mut stroke_width = existing.stroke_width.unwrap_or(resolved.stroke_width);
        let width_response =
            panel_kit::slider_row(ui, "Line px", &mut stroke_width, 0.5..=8.0, 0.0, |value| {
                format!("{value:.2}")
            })
            .on_hover_text("Outline width for this family");
        if width_response.changed() {
            self.style_settings
                .hazards
                .entry(selected_key.clone())
                .or_default()
                .stroke_width = Some((stroke_width * 100.0).round() / 100.0);
            self.rebuild_style_registry();
            ctx.request_repaint();
        }
        if width_response.drag_stopped() || (width_response.changed() && !width_response.dragged())
        {
            self.save_styles();
        }

        let mut family_fill_alpha = existing.fill_alpha.unwrap_or_else(|| {
            resolved
                .fill_alpha
                .unwrap_or(self.style_registry.hazard_global().fill_alpha)
        }) as f32;
        let fill_alpha_response = panel_kit::slider_row(
            ui,
            "Fill alpha",
            &mut family_fill_alpha,
            0.0..=100.0,
            0.0,
            |value| format!("{value:.0}"),
        )
        .on_hover_text("Fill opacity for this family (overrides the global Fill)");
        if fill_alpha_response.changed() {
            self.style_settings
                .hazards
                .entry(selected_key.clone())
                .or_default()
                .fill_alpha = Some(family_fill_alpha.round() as u8);
            self.rebuild_style_registry();
            ctx.request_repaint();
        }
        if fill_alpha_response.drag_stopped()
            || (fill_alpha_response.changed() && !fill_alpha_response.dragged())
        {
            self.save_styles();
        }

        let current_dash = existing.dash.unwrap_or(resolved.dash);
        let mut dash = current_dash;
        panel_kit::row(ui, "Dash", |ui| {
            egui::ComboBox::from_id_salt("appearance_hazard_polygon_dash")
                .selected_text(hazard_dash_label(dash))
                .width(104.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut dash, styles::DashPattern::Solid, "Solid");
                    ui.selectable_value(
                        &mut dash,
                        styles::DashPattern::Dashed {
                            dash: 9.0,
                            gap: 6.0,
                        },
                        "Dashed",
                    );
                    ui.selectable_value(&mut dash, styles::DashPattern::Dotted, "Dotted");
                });
        });
        if dash != current_dash {
            self.style_settings
                .hazards
                .entry(selected_key.clone())
                .or_default()
                .dash = Some(dash);
            self.rebuild_style_registry();
            self.save_styles();
            ctx.request_repaint();
        }
        ui.weak("These style changes apply to live alerts, watches, SPC discussions, and loaded text polygons.");
    }

    /// DATA — acquisition and sources (spec §1): the archive browser, live
    /// poll feeds, a two-line model-store summary, and local file entry
    /// points. Everything that REPLACES or FEEDS the primary volume source
    /// lives here; layers live in LAYERS.
    pub(crate) fn data_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Keep playback attached to the archive controls it operates. The
        // compact redesign briefly left the scrubber above unrelated Data
        // sections and hid tornado browsing behind a mode switch; both made
        // the established archive workflow needlessly hard to find.
        self.remembered_section(ui, "data_archive", "Radar archive", true, |app, ui| {
            app.frame_history_panel(ui, ctx);
            ui.separator();
            app.archive_panel(ui, ctx);
        });
        self.remembered_section(
            ui,
            "data_event_day",
            "Tornado archive (SPC)",
            true,
            |app, ui| {
                app.event_day_section(ui, ctx);
            },
        );
        self.remembered_section(ui, "data_packs", "Data packs", true, |app, ui| {
            app.data_packs_section(ui, ctx);
        });
        self.remembered_section(
            ui,
            "data_community_cases",
            "Community cases",
            false,
            |app, ui| {
                app.community_case_directory_section(ui, ctx);
            },
        );
        self.remembered_section(
            ui,
            "data_radar_coverage",
            "Radar coverage",
            true,
            |app, ui| {
                app.radar_coverage_section(ui, ctx);
            },
        );
        self.remembered_section(
            ui,
            "data_grid_composites",
            "Grid / Composites",
            true,
            |app, ui| {
                ui.weak("Official gridded radar layers that draw on the map.");
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .button("Show Italy DPC VMI")
                        .on_hover_text("Add the current Italian national VMI radar composite from the raw DPC GeoTIFF")
                        .clicked()
                    {
                        app.add_italy_dpc_layer(ItalyDpcMapProduct::Vmi, ctx);
                    }
                    if ui
                        .button("Show Taiwan CWA Reflectivity")
                        .on_hover_text(
                            "Add Taiwan CWA O-A0059-001 numeric composite reflectivity",
                        )
                        .clicked()
                    {
                        app.add_taiwan_cwa_layer(ctx);
                    }
                    ui.menu_button("Products", |ui| {
                        ui.set_min_width(220.0);
                        for product in ItalyDpcMapProduct::ALL {
                            if ui.button(product.label()).clicked() {
                                app.add_italy_dpc_layer(product, ctx);
                                ui.close();
                            }
                        }
                    })
                    .response
                    .on_hover_text("Protezione Civile / Radar-DPC national raw GeoTIFF composites");
                });
                if let Some(layer) = &app.italy_dpc_layer {
                    ui.weak(format!("Active: Italy DPC {}", layer.product.short_label()));
                }
                if app.taiwan_cwa_layer.is_some() {
                    ui.weak("Active: Taiwan CWA Composite Reflectivity");
                }
            },
        );
        self.remembered_section(ui, "data_live_feeds", "Live feeds", true, |app, ui| {
            app.live_feeds_section(ui, ctx);
        });
        self.remembered_section(ui, "data_model_store", "Model store", true, |app, ui| {
            let newest = app
                .model_dock
                .as_ref()
                .and_then(|dock| dock.newest_run())
                .map(|(model, run, hours)| format!("{model} {run} · {hours} hrs in store"))
                .unwrap_or_else(|| "no runs in store".to_owned());
            ui.weak(newest)
                .on_hover_text("Newest model run in the local store (rusty-weather rw-store)");
            if ui
                .button("Download…")
                .on_hover_text(
                    "Open the Model window's Download section: Fetch latest one-click ingest, or any run/hours with size + compute estimates",
                )
                .clicked()
            {
                app.model_enabled = true;
                app.open_viewer(dock::WorkspacePane::Model);
                app.model_download_open = true;
            }
        });
        self.remembered_section(ui, "data_local", "Local files", true, |app, ui| {
            let _ = (&app, &ui);
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            ui.horizontal(|ui| {
                if ui
                    .button("Open radar file…")
                    .on_hover_text(
                        "Open local radar file(s): NEXRAD Level II (.ar2v/.gz/.msg31), a DORADE \
                         sweepfile (swp.*), ODIM_H5/CfRadial (.h5/.hdf/.nc), or a DOW/COW/RaXPol deployment .zip. \
                         Select multiple same-scan ODIM files to merge split DBZH/TH/VRADH archive downloads.",
                    )
                    .clicked()
                    && app.load_receiver.is_none()
                    && let Some(paths) = rfd::FileDialog::new()
                        .add_filter("All radar files (swp.* sweepfiles)", &["*"])
                        .add_filter(
                            "Level II / archives",
                            &[
                                "zip", "ar2v", "gz", "bz2", "raw", "msg31", "v06", "v08", "h5",
                                "hdf", "hd5", "nc",
                            ],
                        )
                        .set_title("Open radar data")
                        .pick_files()
                {
                    app.start_local_volume_file_selection(paths, ui.ctx());
                }
                if ui
                    .button("Open folder…")
                    .on_hover_text(
                        "Open a whole deployment folder: per-sweep DORADE files group into \
                         volumes automatically; Level II files inside load too",
                    )
                    .clicked()
                    && let Some(dir) = rfd::FileDialog::new()
                        .set_title("Open a radar deployment folder")
                        .pick_folder()
                {
                    app.start_local_volume_load(dir, ui.ctx());
                }
            });
            #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
            ui.weak("Native file dialogs are unavailable on this platform.");
        });
    }

    /// Read-only discovery of deliberate signed case-room publications.
    fn community_case_directory_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self
            .community_case_workspace
            .poll(&mut self.community_case_browser)
        {
            ctx.request_repaint();
        }
        if self.community_case_browser.poll() {
            ctx.request_repaint();
        }
        if self.community_case_browser.is_loading()
            || self.community_case_workspace.has_background_work()
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        let community_settings = &self.app_settings.community_cache;
        let enabled = community_settings.phase1_active() && community_settings.explicit_case_rooms;
        ui.weak(
            "Browse deliberate, origin-signed case publications. Opening this section does not search, publish, upload, or expose local files.",
        );
        if let Some(runtime) = &self.community_relay_runtime {
            let relay = runtime.status();
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Private Community Sharing").strong());
                ui.weak(relay.summary());
            });
            ui.weak(format!(
                "{} eligible retained object{} · {} advertised · {} recovered",
                relay.verified_seed_objects,
                if relay.verified_seed_objects == 1 {
                    ""
                } else {
                    "s"
                },
                relay.advertised_objects,
                relay.recovered_objects,
            ));
        }
        if !enabled {
            ui.weak(
                "Enable Community Cache and “Published case rooms and artifacts” in Settings to browse.",
            );
        }

        let mut refresh_clicked = false;
        let mut more_clicked = false;
        ui.horizontal_wrapped(|ui| {
            let label = if self.community_case_browser.cases().is_empty() {
                "Browse"
            } else {
                "Refresh"
            };
            refresh_clicked = ui
                .add_enabled(
                    enabled && !self.community_case_browser.is_loading(),
                    egui::Button::new(label),
                )
                .on_disabled_hover_text(
                    "Case-room browsing must be enabled and no directory request may be active",
                )
                .clicked();
            if self.community_case_browser.is_loading() {
                ui.spinner();
            }
            if self.community_case_browser.next_after().is_some() {
                more_clicked = ui
                    .add_enabled(
                        enabled && !self.community_case_browser.is_loading(),
                        egui::Button::new("More"),
                    )
                    .clicked();
            }
        });

        if refresh_clicked {
            let result = crate::community_cache::CommunityCacheClient::from_settings(
                &self.app_settings.community_cache,
                settings::community_cache_dir(),
            )
            .and_then(|client| self.community_case_browser.refresh(client));
            if let Err(error) = result {
                self.community_case_browser.report_error(&error);
            }
        } else if more_clicked {
            let result = crate::community_cache::CommunityCacheClient::from_settings(
                &self.app_settings.community_cache,
                settings::community_cache_dir(),
            )
            .and_then(|client| self.community_case_browser.load_more(client));
            if let Err(error) = result {
                self.community_case_browser.report_error(&error);
            }
        }

        if let Some(status) = self.community_case_browser.status() {
            let color = if status.contains("failed") || status.contains("stopped") {
                egui::Color32::from_rgb(244, 194, 92)
            } else {
                ui.visuals().weak_text_color()
            };
            ui.colored_label(color, status);
        }

        let open_case_id = self
            .community_case_browser
            .open_case_id()
            .map(str::to_owned);
        let viewed_artifact = self
            .community_case_browser
            .viewed_artifact()
            .map(|(case_id, artifact_id)| (case_id.to_owned(), artifact_id.to_owned()));
        let mut toggle_case = None;
        let mut toggle_artifact = None;
        let visible_cases = self.community_case_browser.cases().to_vec();
        for signed in &visible_cases {
            let case = &signed.manifest;
            let is_open = open_case_id.as_deref() == Some(case.case_id.as_str());
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(&case.title).strong());
                    ui.weak(format!(
                        "{} artifact{}",
                        case.artifacts.len(),
                        if case.artifacts.len() == 1 { "" } else { "s" }
                    ));
                    if ui.button(if is_open { "Close" } else { "Open" }).clicked() {
                        toggle_case = Some(case.case_id.clone());
                    }
                });
                ui.weak(format!(
                    "{} — {}",
                    Self::community_case_time_label(case.event_start_unix),
                    Self::community_case_time_label(case.event_end_unix)
                ));
                let source_summary = case
                    .sources
                    .iter()
                    .map(|source| format!("{} {}", source.model, source.run))
                    .collect::<Vec<_>>()
                    .join(" · ");
                ui.weak(source_summary);

                if is_open {
                    ui.separator();
                    ui.weak(format!(
                        "Published {} · retained through {} · signed by {}",
                        Self::community_case_time_label(case.published_unix),
                        Self::community_case_time_label(case.retain_until_unix),
                        signed.signature.signing_key_id
                    ));

                    ui.label(egui::RichText::new("Sources").strong());
                    for source in &case.sources {
                        ui.label(format!("{} · run {}", source.model, source.run));
                        let snapshot = source.snapshot_id.get(..12).unwrap_or(&source.snapshot_id);
                        let grid = source.grid_hash.get(..12).unwrap_or(&source.grid_hash);
                        ui.weak(format!("snapshot {snapshot}… · grid {grid}…"));
                        for provenance in &source.source_provenance {
                            ui.weak(format!(
                                "{} · roles: {} · products: {}",
                                provenance.provider,
                                provenance.roles.join(", "),
                                provenance.products.join(", ")
                            ));
                        }
                    }

                    if !case.attributions.is_empty() {
                        ui.label(egui::RichText::new("Attribution").strong());
                    }
                    for attribution in &case.attributions {
                        ui.label(egui::RichText::new(&attribution.provider).strong());
                        ui.weak(&attribution.notice);
                        ui.weak(format!("License: {}", attribution.license));
                        ui.horizontal_wrapped(|ui| {
                            ui.hyperlink_to("Source", &attribution.source_url);
                            ui.hyperlink_to("License", &attribution.license_url);
                            ui.hyperlink_to("Terms", &attribution.terms_url);
                        });
                        if !attribution.disclaimer.is_empty() {
                            ui.weak(&attribution.disclaimer);
                        }
                    }
                    for notice in &case.modification_notices {
                        ui.weak(format!("Modification notice: {notice}"));
                    }

                    ui.label(egui::RichText::new("Artifacts").strong());
                    for artifact in &case.artifacts {
                        let viewed = viewed_artifact.as_ref().is_some_and(
                            |(view_case_id, view_artifact_id)| {
                                view_case_id == &case.case_id
                                    && view_artifact_id == &artifact.artifact_id
                            },
                        );
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!(
                                "{} · {}",
                                artifact.artifact_id,
                                Self::community_case_artifact_label(artifact.artifact_type)
                            ));
                            if ui.button(if viewed { "Hide" } else { "View" }).clicked() {
                                toggle_artifact =
                                    Some((case.case_id.clone(), artifact.artifact_id.clone()));
                            }
                        });
                        if viewed {
                            ui.indent(
                                ("case-artifact", &case.case_id, &artifact.artifact_id),
                                |ui| {
                                    self.community_case_workspace.artifact_ui(
                                        ui,
                                        ctx,
                                        &self.app_settings.community_cache,
                                        signed,
                                        artifact,
                                    );
                                },
                            );
                        }
                    }
                }
            });
        }
        if let Some(case_id) = toggle_case {
            self.community_case_browser.toggle_case(&case_id);
        }
        if let Some((case_id, artifact_id)) = toggle_artifact {
            self.community_case_browser
                .toggle_artifact(&case_id, &artifact_id);
        }
        self.community_case_workspace
            .publication_ui(ui, &self.app_settings.community_cache);
    }

    fn community_case_time_label(unix: i64) -> String {
        chrono::DateTime::<chrono::Utc>::from_timestamp(unix, 0).map_or_else(
            || "invalid signed time".to_owned(),
            |time| time.format("%Y-%m-%d %H:%M UTC").to_string(),
        )
    }

    fn community_case_artifact_label(
        artifact: rw_community_protocol::CaseArtifactType,
    ) -> &'static str {
        match artifact {
            rw_community_protocol::CaseArtifactType::Annotation => "annotation",
            rw_community_protocol::CaseArtifactType::DerivedTable => "derived table",
            rw_community_protocol::CaseArtifactType::Overlay => "overlay",
            rw_community_protocol::CaseArtifactType::RenderedImage => "rendered image",
        }
    }

    /// Display preferences (Settings ▸ Display).
    fn display_settings_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Wrapped, not a kit row: three radios outgrow the control column
        // at 320 pt, and swapping them for a combo would change the
        // interaction (the plan forbids behavior changes).
        ui.horizontal_wrapped(|ui| {
            ui.label("Smoothing");
            let mut changed = false;
            changed |= ui
                .radio_value(&mut self.display_smoothing, SmoothingMode::Native, "Native")
                .on_hover_text(
                    "Raw gates, nearest-gate rendering — full super-res detail (the app's identity), RF purple visible.",
                )
                .changed();
            changed |= ui
                .radio_value(&mut self.display_smoothing, SmoothingMode::Soften, "Soften")
                .on_hover_text(
                    "GR2-style smoothing: a 3×3 binomial kernel over the polar grid, computed once per product on the render worker and drawn through the regular fast path (pans stay fast). Values soften but cell edges remain. RF gates render transparent.",
                )
                .changed();
            changed |= ui
                .radio_value(
                    &mut self.display_smoothing,
                    SmoothingMode::Interpolated,
                    "Interpolated",
                )
                .on_hover_text(
                    "Inter-gate interpolation: bilinear upsampling on the polar grid — synthetic radials and gates between the native ones (e.g. a 1° × 500 m international cut renders at 0.25° × 250 m), computed once per product and drawn through the regular fast path (pans stay fast). Continuous look without cell edges; coarse international cuts benefit most. Echo coverage never grows, velocity never blends across folds/couplets, and RF gates render transparent.",
                )
                .changed();
            if changed {
                self.app_settings.smooth_display_mode =
                    self.display_smoothing.settings_str().to_owned();
                // Keep the legacy bool meaningful for older builds reading
                // this config: any non-native mode maps back to their
                // Smooth display checkbox.
                self.app_settings.smooth_display =
                    self.display_smoothing != SmoothingMode::Native;
                self.mark_app_settings_dirty();
                ctx.request_repaint();
            }
        });
        self.raster_quality_settings(ui, ctx);
        panel_kit::row(ui, "Theme", |ui| {
            let current = ui_theme::ThemeChoice::from_slug(&self.app_settings.ui_theme);
            let mut picked = None;
            egui::ComboBox::from_id_salt("display_theme")
                .selected_text(current.label())
                .width(118.0)
                .show_ui(ui, |ui| {
                    for option in ui_theme::ThemeChoice::ALL {
                        if ui
                            .selectable_label(current == option, option.label())
                            .clicked()
                        {
                            picked = Some(option);
                        }
                    }
                })
                .response
                .on_hover_text(
                    "Chrome color theme: Slate (blue-grey, cyan accent) or Graphite + amber \
                     (pure neutral, amber accent). Applies immediately; radar palettes and \
                     map layers are unaffected. A custom Brand Kit drives the chrome instead \
                     while one is active.",
                );
            if let Some(option) = picked
                && option != current
            {
                self.app_settings.ui_theme = option.as_slug().to_owned();
                ui_theme::apply_persisted_theme(ctx, &self.app_settings.ui_theme);
                // Rebuild the egui style document — theme() feeds
                // configure_style, per-frame accessors pick it up anyway.
                configure_style(ctx, &self.app_settings);
                self.mark_app_settings_dirty();
                ctx.request_repaint();
            }
        });
        panel_kit::row(ui, "Floating windows", |ui| {
            let reset = ui
                .add_enabled(
                    self.app_settings.floating_window_accent_rgb.is_some(),
                    egui::Button::new("Reset"),
                )
                .on_hover_text("Follow the active Theme or Brand Kit accent");
            let mut changed = false;
            if reset.clicked() {
                self.app_settings.floating_window_accent_rgb = None;
                changed = true;
            }

            let [r, g, b, _] = resolved_floating_window_accent(&self.app_settings).to_array();
            let mut accent = [r, g, b];
            if ui
                .color_edit_button_srgb(&mut accent)
                .on_hover_text(
                    "Color used for floating-window outlines, active title bars, and a restrained window-surface tint so collapsed windows remain visible against the sidebar",
                )
                .changed()
            {
                self.app_settings.floating_window_accent_rgb = Some(accent);
                changed = true;
            }
            ui.weak(if self.app_settings.floating_window_accent_rgb.is_some() {
                "custom"
            } else {
                "theme default"
            });

            if changed {
                configure_style(ctx, &self.app_settings);
                self.mark_app_settings_dirty();
                ctx.request_repaint();
            }
        });
        panel_kit::row(ui, "Units", |ui| {
            let current = self.units();
            let mut picked = None;
            egui::ComboBox::from_id_salt("display_units")
                .selected_text(current.label())
                .width(118.0)
                .show_ui(ui, |ui| {
                    for option in [units::Units::Imperial, units::Units::Metric] {
                        if ui
                            .selectable_label(current == option, option.label())
                            .clicked()
                        {
                            picked = Some(option);
                        }
                    }
                })
                .response
                .on_hover_text(
                    "Readout units: beam heights, distances, temperatures (lowest-beam menu, \
                     cursor inspector, station plots, range circles). BowEcho is US-born so \
                     imperial is the default — metric is this one click.",
                );
            if let Some(option) = picked
                && option != current
            {
                self.app_settings.units = option.slug().to_owned();
                self.mark_app_settings_dirty();
                ctx.request_repaint();
            }
        });
        // "Where is m/s?" lands here first, so say where velocity units
        // actually live when this setting does not govern them — as a
        // collapsed kit disclosure (prose never inline-expanded).
        if let Some(note) = self.velocity_units_note_for_settings() {
            panel_kit::about(
                ui,
                "settings_velocity_units_about",
                "velocity units",
                &[
                    note.as_str(),
                    "A velocity palette's declared Units: header (kt, mph, km/h, m/s) drives the \
                     velocity readout, colorbar ticks, and unit chip — GR2Analyst semantics. Pick or \
                     edit the table under Custom > Appearance > Color tables to change it.",
                ],
            );
        }
        panel_kit::row(ui, "Time zone", |ui| {
            let current = self.time_zone();
            let mut picked = None;
            egui::ComboBox::from_id_salt("display_time_zone")
                .selected_text(current.label())
                .width(118.0)
                .show_ui(ui, |ui| {
                    for option in DisplayTimeZone::ALL {
                        if ui
                            .selectable_label(current == option, option.label())
                            .clicked()
                        {
                            picked = Some(option);
                        }
                    }
                })
                .response
                .on_hover_text(
                    "Display-only time zone for map chips and readouts. Archive keys, \
                     SPC/day logic, and downloads stay UTC so midnight cases do not shift.",
                );
            if let Some(option) = picked
                && option != current
            {
                self.app_settings.time_zone = option.slug().to_owned();
                self.mark_app_settings_dirty();
                ctx.request_repaint();
            }
        });
        panel_kit::row(ui, "Basemap", |ui| {
            let mut changed_style = None;
            egui::ComboBox::from_id_salt("basemap_style")
                .selected_text(self.basemap_style.label())
                .width(118.0)
                .show_ui(ui, |ui| {
                    for style in tiles::TileStyle::ALL {
                        if ui
                            .selectable_label(self.basemap_style == style, style.label())
                            .clicked()
                        {
                            changed_style = Some(style);
                        }
                    }
                });
            if let Some(style) = changed_style
                && style != self.basemap_style
            {
                self.basemap_style = style;
                self.app_settings.basemap_style = style.key().to_owned();
                self.mark_app_settings_dirty();
                ctx.request_repaint();
            }
        });
        let mut basemap_lines_changed = false;
        basemap_lines_changed |= panel_kit::slider_row(
            ui,
            "Line brightness",
            &mut self.app_settings.basemap_line_brightness_percent,
            20..=200,
            0.0,
            |value| format!("{value}%"),
        )
        .on_hover_text("Boundary/admin line intensity on the basemap")
        .changed();
        basemap_lines_changed |= panel_kit::slider_row(
            ui,
            "Line thickness",
            &mut self.app_settings.basemap_line_thickness_percent,
            25..=250,
            0.0,
            |value| format!("{value}%"),
        )
        .on_hover_text("Boundary/admin line width on the basemap")
        .changed();
        if basemap_lines_changed {
            self.app_settings.basemap_line_brightness_percent = self
                .app_settings
                .basemap_line_brightness_percent
                .clamp(20, 200);
            self.app_settings.basemap_line_thickness_percent = self
                .app_settings
                .basemap_line_thickness_percent
                .clamp(25, 250);
            self.mark_app_settings_dirty();
            ctx.request_repaint();
        }
        if panel_kit::slider_row(
            ui,
            "Scroll zoom speed",
            &mut self.app_settings.zoom_speed_percent,
            50..=300,
            0.0,
            |value| format!("{value}%"),
        )
        .on_hover_text(
            "How far one scroll-wheel notch zooms the map. 100% is the classic feel; the default 150% zooms half again faster.",
        )
        .changed()
        {
            self.mark_app_settings_dirty();
        }
        if ui
            .checkbox(
                &mut self.app_settings.basemap_lightweight,
                "Lightweight basemap",
            )
            .on_hover_text(
                "Skip dense county and regional admin detail for low-end systems; states, countries, cities, and weather layers stay visible",
            )
            .changed()
        {
            self.mark_app_settings_dirty();
            ctx.request_repaint();
        }
        if ui
            .checkbox(&mut self.bold_labels, "Bold town labels")
            .on_hover_text(
                "GR2-style callout labels: bold white with a heavy outline, readable over storm cores",
            )
            .changed()
        {
            self.app_settings.bold_labels = self.bold_labels;
            self.mark_app_settings_dirty();
            ctx.request_repaint();
        }
        if ui
            .checkbox(&mut self.app_settings.show_lat_lon_grid, "Lat/lon grid")
            .on_hover_text("Show basemap latitude/longitude grid lines and labels. Hotkey: G")
            .changed()
        {
            self.mark_app_settings_dirty();
            ctx.request_repaint();
        }
        if ui
            .checkbox(&mut self.app_settings.show_tropical, "Tropical cyclones")
            .on_hover_text(
                "Off and network-idle on every launch. Enable for this session to fetch and show active hurricanes/typhoons worldwide (NHC + GDACS + JTWC): storm cards, positions, forecast tracks, intensity, wind radii, and uncertainty areas.",
            )
            .changed()
        {
            self.mark_app_settings_dirty();
            ctx.request_repaint();
        }
        if self.app_settings.show_tropical {
            ui.indent("tropical_panel_toggle", |ui| {
                if ui
                    .checkbox(
                        &mut self.app_settings.show_tropical_panel,
                        "Storm cards window",
                    )
                    .on_hover_text(
                        "Show the floating storm-cards window listing each active storm's vitals. \
                         The map overlay stays on either way; the window's [×] unchecks this.",
                    )
                    .changed()
                {
                    self.mark_app_settings_dirty();
                    ctx.request_repaint();
                }
            });
        }
        if ui
            .checkbox(
                &mut self.app_settings.show_hurricane_hunters,
                "Hurricane Hunters (live HDOB)",
            )
            .on_hover_text(
                "Live USAF and NOAA reconnaissance aircraft from the four official NHC High-Density Observation feeds. Draws a restart-safe recent flight track, spaced flight-level wind barbs, and the newest aircraft position. Polls only while enabled; bulletins older than 12 hours are rejected.",
            )
            .changed()
        {
            self.mark_app_settings_dirty();
            ctx.request_repaint();
        }
        if self.app_settings.show_hurricane_hunters {
            ui.indent("hurricane_hunters_status", |ui| {
                self.tropical.hurricane_hunters.status_ui(ui);
                let newest_position = self
                    .tropical
                    .hurricane_hunters
                    .newest_position(chrono::Utc::now());
                if ui
                    .add_enabled(
                        newest_position.is_some(),
                        egui::Button::new("✈ Find newest aircraft"),
                    )
                    .on_hover_text(
                        "Center the main map on the newest live Hurricane Hunters observation",
                    )
                    .clicked()
                    && let Some((latitude, longitude)) = newest_position
                {
                    self.center_map_on(latitude, longitude);
                    ctx.request_repaint();
                }
                ui.hyperlink_to(
                    "Official NHC reconnaissance page",
                    "https://www.nhc.noaa.gov/recon.php",
                );
            });
        }
        if ui
            .checkbox(&mut self.app_settings.show_radar_status, "Radar status / outages")
            .on_hover_text(
                "Show the selected US NEXRAD/TDWR radar's live operational status from the NWS \
                 (api.weather.gov/radar/stations): operational / degraded / DOWN, plus radar-operator \
                 alarm messages. A radar reporting DOWN is also badged red on its map marker.",
            )
            .changed()
        {
            self.mark_app_settings_dirty();
            ctx.request_repaint();
        }
        if ui
            .selectable_label(self.gbvtd.panel_open, "🌀 TC winds (GBVTD)")
            .on_hover_text(
                "Single-Doppler tropical-cyclone wind retrieval on the loaded radar volume: center the eye and retrieve the storm center, radius of maximum wind, and tangential/radial wind profile (Lee et al. 1999).",
            )
            .clicked()
        {
            self.gbvtd.panel_open = !self.gbvtd.panel_open;
            ctx.request_repaint();
        }
        if ui
            .checkbox(
                &mut self.app_settings.right_click_loads_nearest,
                "Right-click loads closest radar",
            )
            .on_hover_text(
                "Skip the lowest-beam menu: right-clicking the map switches straight \
                 to the nearest WSR-88D. Off (default) keeps the menu of the three \
                 lowest-beam radars; Ctrl+click always jumps to the lowest-beam pick.",
            )
            .changed()
        {
            self.mark_app_settings_dirty();
        }
        if ui
            .checkbox(
                &mut self.app_settings.map_click_drops_coordinate_marker,
                "Click empty map drops coordinate marker",
            )
            .on_hover_text(
                "Off by default: normal map clicks do not leave markers. Turn on to drop \
                 a lat/lon marker whenever a plain left-click misses warnings, reports, \
                 tracks, and radar/feed markers.",
            )
            .changed()
        {
            self.mark_app_settings_dirty();
        }
        panel_kit::row(ui, "Workspace", |ui| {
            if ui
                .button("Reset layout")
                .on_hover_text(
                    "Clear the saved workspace and pane layout, then return the map \
                     to a single full workspace",
                )
                .clicked()
            {
                self.reset_workspace_layout();
                ctx.request_repaint();
            }
        });
        // Data-folder override (field request: limited LOCALAPPDATA
        // space). Restart-applied so live stores never move mid-session.
        panel_kit::row(ui, "Data folder", |ui| {
            // Right-to-left control area: Default at the edge, Change…
            // beside it.
            let has_data_folder_override = !self.app_settings.data_dir.trim().is_empty();
            if ui
                .add_enabled(has_data_folder_override, egui::Button::new("Default"))
                .on_hover_text(if has_data_folder_override {
                    "Return to the platform app-data location (restart to apply)"
                } else {
                    "Already using the platform app-data location"
                })
                .clicked()
            {
                self.reset_data_folder_override_in_memory();
                self.mark_app_settings_dirty();
            }
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            if ui
                .button("Change…")
                .on_hover_text(
                    "Where caches and stores live: Level II cache, model / satellite / \
                     lightning stores, map tiles. Default is your platform's app-data \
                     location. Changes apply on restart; existing data is not moved.",
                )
                .clicked()
                && let Some(dir) = rfd::FileDialog::new()
                    .set_title(format!(
                        "Choose the {} data folder",
                        self.app_settings.brand.resolved_display_name()
                    ))
                    .pick_folder()
            {
                self.set_data_folder_override_in_memory(dir);
                self.mark_app_settings_dirty();
            }
        });
        // The resolved path gets its own truncating status line — paths are
        // exactly the kind of text that used to blow the row width.
        let current = settings::data_dir_override()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "platform default".to_owned());
        let pending = self.app_settings.data_dir.trim();
        let path_line = if !pending.is_empty()
            && Some(pending)
                != settings::data_dir_override()
                    .map(|p| p.display().to_string())
                    .as_deref()
        {
            format!("{pending} (restart to apply)")
        } else {
            current
        };
        panel_kit::status_block(ui, &path_line, None);
    }

    /// Radar-product browser preferences. Kept in its own Settings section so
    /// presentation choices can grow without crowding the operational Radar
    /// panel or changing which products are scientifically available.
    fn radar_product_settings_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        panel_kit::row(ui, "Browser layout", |ui| {
            let selected = if self.app_settings.classic_product_picker {
                "Classic full browser (default)"
            } else {
                "Modern categorized browser"
            };
            let mut picked = None;
            egui::ComboBox::from_id_salt("radar_product_browser_layout")
                .selected_text(selected)
                .width(208.0)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(
                            self.app_settings.classic_product_picker,
                            "Classic full browser (default)",
                        )
                        .on_hover_text(
                            "Full product catalog as compact pre-v0.34.2 abbreviation chips in a wrapping grid",
                        )
                        .clicked()
                    {
                        picked = Some(true);
                    }
                    if ui
                        .selectable_label(
                            !self.app_settings.classic_product_picker,
                            "Modern categorized browser",
                        )
                        .on_hover_text(
                            "Explicit alternate with full product names grouped into collapsible families",
                        )
                        .clicked()
                    {
                        picked = Some(false);
                    }
                });
            if let Some(classic) = picked
                && classic != self.app_settings.classic_product_picker
            {
                self.app_settings.classic_product_picker = classic;
                self.mark_app_settings_dirty();
                ctx.request_repaint();
            }
        });
        ui.weak(
            "This selects the full catalog browser, separate from the always-visible mini Quick Products strip. Product availability, hotkeys, lazy computation, and pane selections remain identical.",
        );

        let mut show_more = self.app_settings.show_derived_products;
        if ui
            .checkbox(&mut show_more, "Show extended products")
            .on_hover_text(
                "Include volume, motion-analysis, dual-pol, precipitation, texture, and quality products in the selected browser layout",
            )
            .changed()
        {
            self.app_settings.show_derived_products = show_more;
            self.sanitize_selection();
            self.clear_texture();
            self.mark_app_settings_dirty();
            ctx.request_repaint();
        }

        let mut remember_tilts = self.app_settings.remember_product_tilts;
        if ui
            .checkbox(&mut remember_tilts, "Restore last tilt per product")
            .on_hover_text(
                "Remember separate selected tilts for REF, VEL, CC, ZDR, and other products",
            )
            .changed()
        {
            self.app_settings.remember_product_tilts = remember_tilts;
            if !remember_tilts {
                self.product_cut_memory.clear();
            }
            self.mark_app_settings_dirty();
        }
    }

    /// Radar raster quality (Settings ▸ Display): the supersample resolution
    /// the frame you're viewing is rasterized at, plus the power-user whole-loop
    /// toggle and its memory estimate. Standard + toggle-off is bit-identical to
    /// pre-feature builds.
    fn raster_quality_settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        panel_kit::row(ui, "Raster quality", |ui| {
            let current = self.raster_quality();
            let mut picked = None;
            egui::ComboBox::from_id_salt("raster_quality")
                .selected_text(current.label())
                .width(140.0)
                .show_ui(ui, |ui| {
                    for option in settings::RasterQuality::ALL {
                        if ui
                            .selectable_label(current == option, option.label())
                            .clicked()
                        {
                            picked = Some(option);
                        }
                    }
                })
                .response
                .on_hover_text(
                    "Pixel resolution the radar polar data is rasterized into for the frame you are \
                     viewing (the live frame, or the frame you stop the loop on). Higher is sharper when \
                     you zoom in and in native screenshots — real added gate detail, not upscaling — at a \
                     memory/CPU cost that grows with the pixel count (~4 MB Standard, ~16 MB High, ~64 MB \
                     Ultra per frame). While a loop plays it keeps rendering at Standard unless the toggle \
                     below is on.",
                );
            if let Some(option) = picked
                && option != current
            {
                self.app_settings.raster_quality = option.as_slug().to_owned();
                self.mark_app_settings_dirty();
                ctx.request_repaint();
            }
        });
        let mut whole_loop = self.app_settings.raster_high_res_whole_loop;
        if ui
            .checkbox(&mut whole_loop, "Apply high-res to the whole loop")
            .on_hover_text(
                "Render EVERY loop frame at the chosen quality (not just the frame you stop on) \
                 and raise the loop cache budget to hold them. Memory-heavy on long loops — see \
                 the estimate. This warns, it does not block: you can run a full Ultra loop and \
                 accept the cost.",
            )
            .changed()
        {
            self.app_settings.raster_high_res_whole_loop = whole_loop;
            self.mark_app_settings_dirty();
            ctx.request_repaint();
        }
        if self.app_settings.raster_high_res_whole_loop {
            let quality = self.raster_quality();
            let frames = self.primary.history.len();
            let note = if frames > 0 {
                format!(
                    "≈ {} for the current {frames}-frame loop at {}",
                    crate::raster_quality::format_estimate_bytes(
                        self.whole_loop_cache_estimate_bytes()
                    ),
                    quality.label(),
                )
            } else {
                format!(
                    "≈ {} per frame at {} × loop length",
                    crate::raster_quality::format_estimate_bytes(
                        quality.per_frame_estimate_bytes()
                    ),
                    quality.label(),
                )
            };
            ui.weak(note);
        }
    }

    /// Hotkey reference (Settings ▸ Hotkeys).
    fn hotkeys_section(&mut self, ui: &mut egui::Ui) {
        ui.weak("PgUp/PgDn or [ / ] timeline · Space play");
        // Plain words, not ←→↑↓ arrow glyphs: the proportional UI font has
        // none of U+2190-2193 and rendered all four as tofu (same finding
        // as the TILT-section hint; fonts.rs coverage test pins it). Words
        // also match the timeline line above.
        ui.weak("Left/Right product · Up/Down tilt (focused pane)");
        ui.weak("Home reset view · End newest · Shift+X cross-section");
        let mut bindings: Vec<(&String, &String)> =
            self.app_settings.product_hotkeys.iter().collect();
        bindings.sort_by_key(|(key, _)| product_hotkey_sort_key(key));
        for (key, label) in bindings {
            ui.monospace(format!("{key}  ->  {label}"));
        }
        if let Some(path) = settings::AppSettings::config_path() {
            ui.weak(format!("customize in {}", path.display()));
        }
    }

    /// Settings ▸ Debug cases — tiny repro launchers for known radar bugs.
    fn alert_sound_path_label(&self) -> String {
        sound_path_label(&self.app_settings.alert_sound_path)
    }

    fn radar_update_sound_path_label(&self) -> String {
        sound_path_label(&self.app_settings.radar_update_sound_path)
    }

    /// Settings: visual and audible warning latches, family-scoped.
    fn alert_settings_section(&mut self, ui: &mut egui::Ui) {
        if ui
            .checkbox(
                &mut self.app_settings.alert_flash_enabled,
                "Flash new warnings until acknowledged",
            )
            .on_hover_text(
                "Mark newly issued current warnings as NEW and flash the top-bar alert chip until clicked or acknowledged",
            )
            .changed()
        {
            if !self.app_settings.alert_flash_enabled {
                self.unacknowledged_hazard_event_ids.clear();
            }
            self.mark_app_settings_dirty();
        }
        ui.add_enabled_ui(self.app_settings.alert_flash_enabled, |ui| {
            ui.label("Flashing warning types");
            for (family, label) in ALERT_SOUND_FAMILY_OPTIONS {
                let mut enabled =
                    alert_family_enabled(&self.app_settings.alert_flash_families, family);
                if ui.checkbox(&mut enabled, *label).changed() {
                    set_alert_family_enabled(
                        &mut self.app_settings.alert_flash_families,
                        family,
                        enabled,
                    );
                    self.mark_app_settings_dirty();
                }
            }
            if self.app_settings.alert_flash_families.is_empty() {
                ui.weak("All supported warning types flash.");
            }
        });
        ui.separator();
        if ui
            .checkbox(
                &mut self.app_settings.alert_sound_enabled,
                "Play sound for new warnings",
            )
            .on_hover_text(
                "Opt-in audible cue when a newly issued current warning is latched as NEW in the Alerts tab",
            )
            .changed()
        {
            self.mark_app_settings_dirty();
        }

        ui.add_enabled_ui(self.app_settings.alert_sound_enabled, |ui| {
            panel_kit::row(ui, "Sound", |ui| {
                // Right-to-left control area: Test at the edge, then
                // System, then Choose WAV… (the path gets its own
                // truncating line below).
                if ui
                    .button("Test")
                    .on_hover_text("Play the selected alert sound once")
                    .clicked()
                {
                    self.trigger_alert_sound();
                }
                if ui
                    .button("System")
                    .on_hover_text("Use the platform system alert sound")
                    .clicked()
                {
                    self.app_settings.alert_sound_path.clear();
                    self.mark_app_settings_dirty();
                }
                #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
                if ui
                    .button("Choose WAV…")
                    .on_hover_text("Pick a custom .wav file; empty uses the platform system alert")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_title(format!(
                            "Choose {} warning sound",
                            self.app_settings.brand.resolved_display_name()
                        ))
                        .add_filter("WAV audio", &["wav"])
                        .pick_file()
                {
                    self.app_settings.alert_sound_path = path.display().to_string();
                    self.mark_app_settings_dirty();
                }
            });
            panel_kit::status_block(ui, &self.alert_sound_path_label(), None);
            ui.label("Sound warning types");
            for (family, label) in ALERT_SOUND_FAMILY_OPTIONS {
                let mut enabled =
                    alert_family_enabled(&self.app_settings.alert_sound_families, family);
                if ui.checkbox(&mut enabled, *label).changed() {
                    set_alert_family_enabled(
                        &mut self.app_settings.alert_sound_families,
                        family,
                        enabled,
                    );
                    self.mark_app_settings_dirty();
                }
            }
            if self.app_settings.alert_sound_families.is_empty() {
                ui.weak("All supported warning types are enabled.");
            }
        });
        ui.separator();
        if ui
            .checkbox(
                &mut self.app_settings.radar_update_sound_enabled,
                "Play sound when the radar updates",
            )
            .on_hover_text(
                "Opt-in short cue when the primary live radar installs a new frame; keeps playing while the window is minimized",
            )
            .changed()
        {
            self.mark_app_settings_dirty();
        }
        ui.add_enabled_ui(self.app_settings.radar_update_sound_enabled, |ui| {
            panel_kit::row(ui, "Sound", |ui| {
                if ui
                    .button("Test")
                    .on_hover_text("Play the selected radar-updated sound once")
                    .clicked()
                {
                    let _ =
                        alert_audio::play_radar_update(&self.app_settings.radar_update_sound_path);
                }
                if ui
                    .button("System")
                    .on_hover_text("Use the platform system sound")
                    .clicked()
                {
                    self.app_settings.radar_update_sound_path.clear();
                    self.mark_app_settings_dirty();
                }
                #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
                if ui
                    .button("Choose WAV…")
                    .on_hover_text("Pick a custom .wav file; empty uses the platform system sound")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_title(format!(
                            "Choose {} radar-updated sound",
                            self.app_settings.brand.resolved_display_name()
                        ))
                        .add_filter("WAV audio", &["wav"])
                        .pick_file()
                {
                    self.app_settings.radar_update_sound_path = path.display().to_string();
                    self.mark_app_settings_dirty();
                }
            });
            panel_kit::status_block(ui, &self.radar_update_sound_path_label(), None);
        });
    }

    fn debug_cases_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.load_receiver.is_some() {
            ui.weak("Load in progress");
        }
        for case in DEBUG_ARCHIVE_CASES {
            let clicked = ui
                .add_enabled(self.load_receiver.is_none(), egui::Button::new(case.label))
                .on_hover_text(case.description)
                .clicked();
            if clicked {
                self.start_debug_archive_case(*case, ctx);
            }
        }
    }

    fn brand_settings_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let original = self.app_settings.brand.clone();
        let mut edited = original.clone();
        let mut changed = false;
        let mut preset_replaced = false;
        let mut status_message = None;

        ui.horizontal_wrapped(|ui| {
            ui.label("Preset");
            let mut selected = edited.preset;
            egui::ComboBox::from_id_salt("brand_preset")
                .selected_text(selected.label())
                .width(220.0)
                .show_ui(ui, |ui| {
                    for preset in settings::BrandPreset::BUILT_INS {
                        ui.selectable_value(&mut selected, preset, preset.label());
                    }
                    if edited.preset == settings::BrandPreset::Custom {
                        ui.selectable_value(
                            &mut selected,
                            settings::BrandPreset::Custom,
                            settings::BrandPreset::Custom.label(),
                        );
                    }
                });
            if selected != edited.preset {
                edited = match selected {
                    settings::BrandPreset::Custom => {
                        let mut custom = edited.clone();
                        custom.mark_custom();
                        custom
                    }
                    preset => settings::BrandConfig::preset(preset),
                };
                changed = true;
                preset_replaced = true;
                status_message = Some(format!("Applied {} preset", selected.label()));
            }
            if ui.button("Reset preset").clicked() {
                let preset = match edited.preset {
                    settings::BrandPreset::GenericBrandedApp => {
                        settings::BrandPreset::GenericBrandedApp
                    }
                    settings::BrandPreset::BowEcho | settings::BrandPreset::Custom => {
                        settings::BrandPreset::BowEcho
                    }
                };
                edited = settings::BrandConfig::preset(preset);
                changed = true;
                preset_replaced = true;
                status_message = Some(format!("Reset to {}", preset.label()));
            }
        });
        ui.weak(
            "The generic branded preset is a neutral starter skin. Add your own identity, links, palette, logo/icon, watermark, and attribution before distributing a branded build.",
        );

        ui.separator();
        ui.strong("Identity");
        egui::Grid::new("brand_identity_grid")
            .num_columns(2)
            .spacing([10.0, 5.0])
            .show(ui, |ui| {
                changed |= brand_text_field(ui, "Display name", &mut edited.display_name);
                changed |= brand_text_field(ui, "Short name", &mut edited.short_name);
                changed |= brand_text_field(ui, "Organization", &mut edited.organization);
                changed |= brand_text_field(ui, "Tagline", &mut edited.tagline);
                changed |= brand_text_field(
                    ui,
                    "Filename prefix",
                    &mut edited.screenshot_filename_prefix,
                );
                changed |= brand_text_field(ui, "Output folder", &mut edited.output_folder_label);
            });
        ui.weak(format!(
            "Resolved media path: Pictures/{} · files start {}_",
            edited.output_folder_name(),
            edited.filename_prefix()
        ));

        ui.separator();
        ui.strong("Links");
        egui::Grid::new("brand_links_grid")
            .num_columns(2)
            .spacing([10.0, 5.0])
            .show(ui, |ui| {
                changed |= brand_text_field(ui, "Website", &mut edited.website_url);
                changed |= brand_text_field(ui, "Repository", &mut edited.repo_url);
                changed |= brand_text_field(ui, "Releases", &mut edited.releases_url);
                changed |= brand_text_field(ui, "Support", &mut edited.support_url);
                changed |= brand_text_field(ui, "Donate", &mut edited.donate_url);
                changed |= brand_text_field(ui, "Contact", &mut edited.contact_url);
                changed |= brand_text_field(ui, "Privacy", &mut edited.privacy_url);
            });
        ui.horizontal_wrapped(|ui| {
            for (label, url) in [
                ("Website", edited.website_url.as_str()),
                ("Repository", edited.repo_url.as_str()),
                ("Releases", edited.releases_url.as_str()),
                ("Support", edited.support_url.as_str()),
                ("Donate", edited.donate_url.as_str()),
                ("Contact", edited.contact_url.as_str()),
                ("Privacy", edited.privacy_url.as_str()),
            ] {
                if let Some(url) = safe_brand_link(url)
                    && ui.small_button(label).clicked()
                {
                    ctx.open_url(egui::OpenUrl::new_tab(url));
                }
            }
        });

        ui.separator();
        ui.strong("Storage namespace");
        changed |= ui
            .checkbox(
                &mut edited.use_custom_storage_namespace,
                "Use branded storage namespace after restart",
            )
            .on_hover_text(
                "Opt-in only. config.json and styles.json stay in the legacy BowEcho root; caches/stores/user data use the branded namespace after restart.",
            )
            .changed();
        ui.horizontal(|ui| {
            ui.label("Namespace");
            changed |= ui
                .add(egui::TextEdit::singleline(&mut edited.storage_namespace).desired_width(180.0))
                .changed();
            let resolved = settings::sanitize_namespace(&edited.storage_namespace)
                .unwrap_or_else(|| settings::DEFAULT_STORAGE_NAMESPACE.to_owned());
            ui.weak(format!("-> {resolved}"));
        });
        ui.weak(
            "No existing %APPDATA%/bowecho data is moved or deleted. Import storage copies a selected tree, skips symlinks, and never overwrites destination files.",
        );
        ui.horizontal_wrapped(|ui| {
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            {
                let import_enabled = edited.use_custom_storage_namespace
                    && edited.effective_storage_namespace().is_some();
                if ui
                    .add_enabled(import_enabled, egui::Button::new("Import storage..."))
                    .on_hover_text(
                        "Choose an existing BowEcho data root. Files are copied non-destructively into the configured namespace.",
                    )
                    .clicked()
                    && let Some(source) = rfd::FileDialog::new()
                        .set_title("Import existing app storage")
                        .pick_folder()
                    && let Some(namespace) = edited.effective_storage_namespace()
                    && let Some(destination) = settings::storage_root_for_namespace(&namespace)
                {
                    match settings::import_storage_tree(&source, &destination) {
                        Ok(summary) => {
                            status_message = Some(format!(
                                "Imported storage: {} files copied, {} skipped, {} folders created",
                                summary.files_copied,
                                summary.files_skipped,
                                summary.directories_created
                            ));
                        }
                        Err(error) => {
                            status_message = Some(format!("Storage import failed: {error}"));
                        }
                    }
                }
            }
            #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
            {
                ui.weak("Storage import folder picker is unavailable on this platform.");
            }
        });

        ui.separator();
        ui.strong("Palette");
        let fallback = edited.palette_fallback();
        egui::Grid::new("brand_palette_grid")
            .num_columns(4)
            .spacing([10.0, 5.0])
            .show(ui, |ui| {
                changed |= brand_color_field(
                    ui,
                    "Primary",
                    &mut edited.palette.primary,
                    &fallback.primary,
                );
                changed |=
                    brand_color_field(ui, "Accent", &mut edited.palette.accent, &fallback.accent);
                changed |= brand_color_field(
                    ui,
                    "Danger / fire",
                    &mut edited.palette.danger,
                    &fallback.danger,
                );
                changed |= brand_color_field(
                    ui,
                    "Warning",
                    &mut edited.palette.warning,
                    &fallback.warning,
                );
                changed |= brand_color_field(
                    ui,
                    "Success",
                    &mut edited.palette.success,
                    &fallback.success,
                );
                changed |= brand_color_field(
                    ui,
                    "Surface",
                    &mut edited.palette.surface,
                    &fallback.surface,
                );
                changed |= brand_color_field(
                    ui,
                    "Surface alt",
                    &mut edited.palette.surface_alt,
                    &fallback.surface_alt,
                );
                changed |= brand_color_field(ui, "Text", &mut edited.palette.text, &fallback.text);
                changed |= brand_color_field(
                    ui,
                    "Muted text",
                    &mut edited.palette.muted_text,
                    &fallback.muted_text,
                );
                changed |= brand_color_field(
                    ui,
                    "Outline",
                    &mut edited.palette.outline,
                    &fallback.outline,
                );
            });
        if ui.button("Reset palette to preset").clicked() {
            edited.palette = fallback;
            changed = true;
        }

        ui.separator();
        ui.strong("Feature labels");
        egui::Grid::new("brand_features_grid")
            .num_columns(2)
            .spacing([10.0, 5.0])
            .show(ui, |ui| {
                changed |= brand_text_field(ui, "Radar", &mut edited.features.radar);
                changed |= brand_text_field(ui, "Map", &mut edited.features.map);
                changed |= brand_text_field(ui, "Warnings", &mut edited.features.warnings);
                changed |= brand_text_field(ui, "Evacuation", &mut edited.features.evacuation);
                changed |= brand_text_field(ui, "Air quality", &mut edited.features.air_quality);
            });

        ui.separator();
        ui.strong("Assets");
        for field in BrandAssetField::ALL {
            ui.horizontal(|ui| {
                ui.label(field.label());
                let value = field.value_mut(&mut edited.assets);
                let mut text = value.as_deref().unwrap_or_default().to_owned();
                if ui
                    .add(egui::TextEdit::singleline(&mut text).desired_width(250.0))
                    .changed()
                {
                    *value = (!text.trim().is_empty()).then(|| text.trim().to_owned());
                    changed = true;
                }
                #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
                if ui.small_button("Choose...").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter(field.label(), field.extensions())
                        .set_title(format!("Choose {}", field.label()))
                        .pick_file()
                {
                    *value = Some(path.display().to_string());
                    changed = true;
                }
                if ui.small_button("Clear").clicked() {
                    *value = None;
                    changed = true;
                }
                if value
                    .as_deref()
                    .is_some_and(|path| !path.trim().is_empty() && !Path::new(path).is_file())
                {
                    ui.colored_label(egui::Color32::from_rgb(235, 96, 96), "missing");
                }
            });
        }
        ui.weak(
            "Header/watermark/share assets apply at runtime. The launch icon applies on the next start. The executable .ico is build-time only (BOWECHO_APP_ICON_ICO).",
        );

        ui.separator();
        ui.strong("Screenshots");
        if ui
            .checkbox(
                &mut self.app_settings.save_still_screenshots_to_disk,
                "Also save still screenshots to disk",
            )
            .on_hover_text(format!(
                "Still screenshots always copy to the clipboard. When enabled, BowEcho also writes a PNG to Pictures/{}.",
                edited.output_folder_name()
            ))
            .changed()
        {
            self.mark_app_settings_dirty();
        }
        ui.weak(format!(
            "Clipboard copy is always on. Optional PNG folder: Pictures/{}.",
            edited.output_folder_name()
        ));

        ui.separator();
        ui.strong("Social sharing");
        changed |= ui
            .checkbox(&mut edited.sharing.watermark_enabled, "Watermark exports")
            .changed();
        changed |= ui
            .checkbox(&mut edited.sharing.card_enabled, "Share-card metadata")
            .changed();
        ui.horizontal(|ui| {
            ui.label("Layout");
            let before = edited.sharing.layout;
            egui::ComboBox::from_id_salt("brand_share_layout")
                .selected_text(edited.sharing.layout.label())
                .width(92.0)
                .show_ui(ui, |ui| {
                    for layout in settings::ShareLayout::ALL {
                        ui.selectable_value(&mut edited.sharing.layout, layout, layout.label());
                    }
                });
            changed |= edited.sharing.layout != before;
            ui.weak("Aspect presets pad instead of cropping existing context.");
        });
        egui::Grid::new("brand_share_grid")
            .num_columns(2)
            .spacing([10.0, 5.0])
            .show(ui, |ui| {
                changed |= brand_text_field(ui, "Card title", &mut edited.sharing.title);
                changed |= brand_text_field(ui, "Subtitle", &mut edited.sharing.subtitle);
                changed |= brand_text_field(ui, "Site label", &mut edited.sharing.site_label);
                changed |= brand_text_field(ui, "Source footer", &mut edited.sharing.source_footer);
            });

        brand::preview_ui(ui, &edited, &mut self.brand_assets);

        ui.separator();
        ui.horizontal_wrapped(|ui| {
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            {
                let prefix = edited.filename_prefix();
                if ui.button("Export Brand Kit...").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Brand Kit", &["json"])
                        .set_file_name(format!("{prefix}-brand-kit.json"))
                        .set_title(format!(
                            "Export {} Brand Kit",
                            edited.resolved_display_name()
                        ))
                        .save_file()
                {
                    match std::fs::write(&path, edited.to_json()) {
                        Ok(()) => {
                            status_message =
                                Some(format!("Exported Brand Kit to {}", path.display()));
                        }
                        Err(error) => {
                            status_message = Some(format!("Brand Kit export failed: {error}"));
                        }
                    }
                }
                if ui.button("Import Brand Kit...").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Brand Kit", &["json"])
                        .set_title(format!(
                            "Import {} Brand Kit",
                            edited.resolved_display_name()
                        ))
                        .pick_file()
                {
                    match std::fs::read_to_string(&path)
                        .map_err(|error| error.to_string())
                        .and_then(|text| settings::BrandConfig::from_json(&text))
                    {
                        Ok(imported) => {
                            edited = imported;
                            changed = true;
                            preset_replaced = true;
                            status_message =
                                Some(format!("Imported Brand Kit from {}", path.display()));
                        }
                        Err(error) => {
                            status_message = Some(format!("Brand Kit import failed: {error}"));
                        }
                    }
                }
            }
            #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
            {
                ui.weak("Brand Kit file dialogs are unavailable on this platform.");
            }
        });

        if changed {
            if !preset_replaced {
                edited.mark_custom();
            }
            let update_source_changed = original.repo_url != edited.repo_url
                || original.releases_url != edited.releases_url;
            self.app_settings.brand = edited;
            configure_style(ctx, &self.app_settings);
            self.brand_assets.clear();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(brand::window_title(
                &self.app_settings.brand,
            )));
            if update_source_changed {
                self.update_available = None;
                self.update_check_rx.cancel();
                self.start_update_check(ctx);
            }
            if let Err(error) = self.app_settings.save() {
                status_message = Some(format!("Brand settings save failed: {error}"));
            }
        }
        if let Some(message) = status_message {
            self.status = message;
        }
    }

    /// Settings tab: everything volume-independent, set once per session.
    /// ⚙ Settings — collapsible sections, open-state remembered across
    /// restarts (spec §1). A future Appearance section (style registry
    /// surfaces, docs/customization-spec.md §5) slots in beside these.
    pub(crate) fn settings_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // One-shot expand path: the top bar's "vX.Y.Z available" chip lands
        // here with the updater fold forced open (same pattern as the color
        // picker's `open_color_tables_request`).
        if self.open_update_section_request {
            self.open_update_section_request = false;
            self.set_section_open("settings_security_updates", true);
        }
        if let Some(persistence) = self.settings_persistence.status_view(Instant::now()) {
            let color = match persistence.level {
                settings_persistence::PersistenceNoticeLevel::Success => {
                    egui::Color32::from_rgb(108, 218, 142)
                }
                settings_persistence::PersistenceNoticeLevel::Warning => {
                    egui::Color32::from_rgb(244, 194, 92)
                }
                settings_persistence::PersistenceNoticeLevel::Error => {
                    egui::Color32::from_rgb(245, 104, 104)
                }
            };
            ui.colored_label(color, persistence.detail);
            ui.separator();
        }
        self.remembered_section(ui, "settings_display", "Display", true, |app, ui| {
            app.display_settings_section(ui, ctx);
        });
        self.remembered_section(
            ui,
            "settings_radar_products",
            "Radar products",
            false,
            |app, ui| {
                app.radar_product_settings_section(ui, ctx);
            },
        );
        self.remembered_section(
            ui,
            "settings_brand",
            "App Identity / Brand Kit",
            false,
            |app, ui| {
                app.brand_settings_section(ui, ctx);
            },
        );
        // One-shot expand path: the PRODUCTS color picker's "Edit…" lands
        self.remembered_section(
            ui,
            "settings_security_updates",
            "Security & updates",
            false,
            |app, ui| {
                app.security_updates_section(ui, ctx);
            },
        );
        self.remembered_section(ui, "settings_hotkeys", "Hotkeys", false, |app, ui| {
            app.hotkeys_section(ui);
        });
        self.remembered_section(ui, "settings_alerts", "Alerts", false, |app, ui| {
            app.alert_settings_section(ui);
        });
        self.remembered_section(
            ui,
            "settings_backup",
            "Settings backup",
            false,
            |app, ui| {
                app.settings_backup_section(ui, ctx);
            },
        );
        self.remembered_section(
            ui,
            "settings_community_cache",
            "Community Cache",
            false,
            |app, ui| {
                app.community_cache_settings_section(ui);
            },
        );
        self.remembered_section(
            ui,
            "settings_public_origin_federation",
            "Public origin federation",
            false,
            |app, ui| {
                app.federation_settings_section(ui);
            },
        );
        self.remembered_section(
            ui,
            "settings_generation_publication",
            "Owner generation publication",
            false,
            |app, ui| {
                app.generation_publication_settings_section(ui);
            },
        );
        self.remembered_section(
            ui,
            "settings_performance",
            "Performance",
            false,
            |app, ui| {
                app.stats_panel(ui);
            },
        );
        self.remembered_section(
            ui,
            "settings_debug_cases",
            "Debug cases",
            false,
            |app, ui| {
                app.debug_cases_section(ui, ctx);
            },
        );
        self.remembered_section(ui, "settings_model", "Model", false, |app, ui| {
            app.model_settings_section(ui, ctx);
        });
    }

    /// Settings ▸ Model — the settings-class controls evicted from the
    /// Layers fold (ui-refresh proposal section 4 step 4): the app-wide
    /// master switch, the disk-retention policy, and a store-path readout.
    fn settings_backup_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        let _ = ctx;
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        let display_name = self.app_settings.brand.resolved_display_name().to_owned();
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        let filename_prefix = self.app_settings.brand.filename_prefix();
        ui.horizontal_wrapped(|ui| {
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            {
                if fixed_action_button(ui, "Export config...", 112.0)
                    .on_hover_text("Save the current config.json preferences to a backup file")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter(format!("{display_name} config"), &["json"])
                        .set_file_name(format!("{filename_prefix}-config.json"))
                        .set_title(format!("Export {display_name} settings"))
                        .save_file()
                {
                    match settings::atomic_write_json_with_backup_validator(
                        &path,
                        &self.app_settings,
                        settings::MAX_JSON_DOCUMENT_BYTES,
                        |text| serde_json::from_str::<settings::AppSettings>(text).is_ok(),
                    ) {
                        Ok(()) => self.status = format!("Exported settings to {}", path.display()),
                        Err(error) => self.status = format!("Settings export failed: {error}"),
                    }
                }
                if fixed_action_button(ui, "Import config...", 112.0)
                    .on_hover_text(
                        format!(
                            "Replace config.json from a backup; restart {display_name} so every setting rehydrates cleanly"
                        ),
                    )
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter(format!("{display_name} config"), &["json"])
                        .set_title(format!("Import {display_name} settings"))
                        .pick_file()
                {
                    self.import_app_settings_from_path(&path);
                    ctx.request_repaint();
                }
                if fixed_action_button(ui, "Export appearance...", 136.0)
                    .on_hover_text("Save warning polygon, map, radar-age, and layer style overrides")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter(format!("{display_name} styles"), &["json"])
                        .set_file_name(format!("{filename_prefix}-styles.json"))
                        .set_title(format!("Export {display_name} appearance"))
                        .save_file()
                {
                    self.export_styles_to_path(&path);
                }
                if fixed_action_button(ui, "Import appearance...", 136.0)
                    .on_hover_text("Load warning polygon, map, radar-age, and layer style overrides")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter(format!("{display_name} styles"), &["json"])
                        .set_title(format!("Import {display_name} appearance"))
                        .pick_file()
                {
                    self.import_styles_from_path(&path, ctx);
                }
            }
            #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
            {
                ui.weak("Native settings backup dialogs are unavailable on this platform.");
            }
        });
        if let Some(path) = settings::AppSettings::config_path() {
            ui.weak(format!("Config: {}", path.display()));
        }
        if let Some(path) = styles::styles_path() {
            ui.weak(format!("Appearance: {}", path.display()));
        }
    }

    fn generation_publication_settings_section(&mut self, ui: &mut egui::Ui) {
        let original = self.app_settings.generation_publication.clone();
        let mut edited = original.clone();
        let warning = egui::Color32::from_rgb(244, 194, 92);

        ui.weak(
            "Advanced owner workflow for publishing a complete processed private WRF/ArWen rw-store generation to one trusted Rusty Weather origin over conventional authenticated HTTPS.",
        );
        ui.weak(
            "This setting is independent of Community Cache. It never uploads through TURN, never contacts another user, and never starts or resumes publication when BowEcho opens.",
        );
        ui.checkbox(
            &mut edited.enabled,
            "Enable explicit owner generation publication",
        );
        ui.separator();

        ui.label("Trusted origin ID");
        ui.add_enabled(
            false,
            egui::TextEdit::singleline(&mut edited.trusted_origin_id)
                .desired_width(ui.available_width()),
        )
        .on_hover_text(
            "The initial release deliberately admits only the compiled hetzner-primary trust slot.",
        );
        ui.label("Trusted Rusty Weather HTTPS origin");
        ui.add(
            egui::TextEdit::singleline(&mut edited.trusted_origin_url)
                .hint_text("https://models.example.org")
                .desired_width(ui.available_width()),
        );
        ui.label("Opaque owner principal SHA-256");
        ui.add(
            egui::TextEdit::singleline(&mut edited.owner_principal_sha256)
                .hint_text("64 lowercase hex characters issued by the origin")
                .desired_width(ui.available_width())
                .font(egui::TextStyle::Monospace),
        )
        .on_hover_text(
            "This is an opaque owner binding, not an account name, email, path, token, or secret. It is persisted so Prepare can bind identity while offline.",
        );

        let token_id = egui::Id::new("generation_publication_origin_token_draft");
        let status_id = egui::Id::new("generation_publication_origin_token_status");
        let mut token = ui
            .ctx()
            .data_mut(|data| data.get_temp::<String>(token_id).unwrap_or_default());
        let mut credential_status = ui
            .ctx()
            .data_mut(|data| data.get_temp::<String>(status_id).unwrap_or_default());
        ui.label("Origin owner bearer (OS credential vault only)");
        ui.add(
            egui::TextEdit::singleline(&mut token)
                .password(true)
                .hint_text("Paste short-lived or revocable owner credential")
                .desired_width(ui.available_width()),
        );
        ui.ctx()
            .data_mut(|data| data.insert_temp(token_id, token.clone()));
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !token.trim().is_empty(),
                    egui::Button::new("Save in OS vault"),
                )
                .clicked()
            {
                credential_status =
                    match crate::generation_publication::save_origin_credentials_from_app(
                        &edited, &token,
                    ) {
                        Ok(()) => {
                            token.clear();
                            "Saved an origin-bound owner credential in the OS vault.".to_owned()
                        }
                        Err(error) => format!("Credential was not saved: {error}"),
                    };
            }
            if ui.button("Delete vault credential").clicked() {
                credential_status =
                    match crate::generation_publication::delete_origin_credentials_from_app(&edited)
                    {
                        Ok(true) => {
                            "Deleted the credential for this exact origin binding.".to_owned()
                        }
                        Ok(false) => {
                            "No credential existed for this exact origin binding.".to_owned()
                        }
                        Err(error) => format!("Credential was not deleted: {error}"),
                    };
            }
        });
        ui.ctx().data_mut(|data| data.insert_temp(token_id, token));
        ui.ctx()
            .data_mut(|data| data.insert_temp(status_id, credential_status.clone()));
        if !credential_status.is_empty() {
            ui.weak(&credential_status);
        }

        ui.separator();
        ui.label(egui::RichText::new("Publication bounds").strong());
        egui::Grid::new("generation_publication_bounds")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Maximum generation");
                ui.add(
                    egui::DragValue::new(&mut edited.max_generation_gib)
                        .range(1..=4096)
                        .suffix(" GiB"),
                );
                ui.end_row();
                ui.label("Maximum local spool");
                ui.add(
                    egui::DragValue::new(&mut edited.max_spool_gib)
                        .range(2..=8192)
                        .suffix(" GiB"),
                );
                ui.end_row();
                ui.label("Maximum files");
                ui.add(egui::DragValue::new(&mut edited.max_files).range(3..=65_538));
                ui.end_row();
                ui.label("Maximum chunks");
                ui.add(egui::DragValue::new(&mut edited.max_chunks).range(1..=1_000_000));
                ui.end_row();
                ui.label("Chunk size");
                ui.add(
                    egui::DragValue::new(&mut edited.chunk_mib)
                        .range(1..=64)
                        .suffix(" MiB"),
                );
                ui.end_row();
                ui.label("Maximum manifest");
                ui.add(
                    egui::DragValue::new(&mut edited.max_manifest_mib)
                        .range(1..=32)
                        .suffix(" MiB"),
                );
                ui.end_row();
                ui.label("Maximum retention");
                ui.add(
                    egui::DragValue::new(&mut edited.max_retention_days)
                        .range(1..=366)
                        .suffix(" days"),
                );
                ui.end_row();
                ui.label("Default retention");
                ui.add(
                    egui::DragValue::new(&mut edited.default_retention_days)
                        .range(1..=edited.max_retention_days.max(1))
                        .suffix(" days"),
                );
                ui.end_row();
            });
        ui.weak(
            "Prepare remains available offline but admits conservatively against current total spool usage. Publish fetches and enforces the origin's current advertised limits and owner quota before begin.",
        );

        ui.separator();
        ui.label(egui::RichText::new("Mandatory attribution").strong());
        ui.weak(
            "Review these values for every publication. ECMWF attribution and processing notice are locked and appended automatically whenever ECMWF lineage is present.",
        );
        let text_row = |ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str| {
            ui.label(label);
            ui.add(
                egui::TextEdit::singleline(value)
                    .hint_text(hint)
                    .desired_width(ui.available_width()),
            );
        };
        text_row(
            ui,
            "Provider ID",
            &mut edited.attribution_provider,
            "owner-lab",
        );
        text_row(
            ui,
            "Notice",
            &mut edited.attribution_notice,
            "Required attribution notice",
        );
        text_row(
            ui,
            "Source URL",
            &mut edited.attribution_source_url,
            "https://...",
        );
        text_row(
            ui,
            "License",
            &mut edited.attribution_license,
            "License / owner authorization",
        );
        text_row(
            ui,
            "License URL",
            &mut edited.attribution_license_url,
            "https://...",
        );
        text_row(
            ui,
            "Terms URL",
            &mut edited.attribution_terms_url,
            "https://...",
        );
        text_row(
            ui,
            "Disclaimer",
            &mut edited.attribution_disclaimer,
            "Research use disclaimer",
        );
        text_row(
            ui,
            "Modification notice",
            &mut edited.modification_notice,
            "Processing/subsetting performed",
        );

        ui.separator();
        ui.colored_label(
            warning,
            "Only deep-valid closed rw-store generations are accepted. Raw wrfout, arbitrary files/directories, symlinks/reparse points, unsigned private directories, and PublicProvider grants cannot enter this workflow.",
        );
        ui.weak(
            "Published model/run identity remains byte-exact with run.json and every .rws hour. If another owner already occupies that identity, the origin returns a conflict; re-import under a unique run identity. A future import-time opaque namespace may improve this, but publication never fabricates one or rewrites science bytes.",
        );
        ui.weak(
            "The trusted HTTPS operator sees your connection IP and necessary request metadata. Other BowEcho users do not see your address and Community Cache is not involved.",
        );

        if edited != original {
            self.app_settings.generation_publication = edited;
            if let Some(dock) = self.model_dock.as_mut() {
                dock.set_generation_publication_settings(&self.app_settings.generation_publication);
            }
            self.mark_app_settings_dirty();
        }
    }

    fn community_cache_settings_section(&mut self, ui: &mut egui::Ui) {
        let original = self.app_settings.community_cache.clone();
        let mut edited = original.clone();

        ui.weak(
            "Operational data always uses the verified local cache, R2, then the Rusty Weather / Hetzner HTTPS origin. It never invokes the community relay.",
        );
        ui.weak(
            "Explicit cold-historical recovery can separately use a TURN-only encrypted Community Cache copy after local and R2 miss, then falls back to archival HTTPS or unavailable.",
        );
        ui.separator();

        ui.label("Rusty Weather / Hetzner origin (HTTPS)");
        ui.add(
            egui::TextEdit::singleline(&mut edited.origin_url)
                .hint_text("https://...")
                .desired_width(ui.available_width()),
        )
        .on_hover_text(
            "Public HTTPS base URL only. User-info, query strings, fragments, and embedded tokens are rejected.",
        );

        ui.label("R2 hot-object URL (optional, HTTPS)");
        ui.add(
            egui::TextEdit::singleline(&mut edited.r2_hot_object_url)
                .hint_text("https://...")
                .desired_width(ui.available_width()),
        )
        .on_hover_text("Optional public HTTPS base URL for frequently requested cache objects");

        ui.label("Legacy origin key · rw-origin-v1 (Ed25519, base64)");
        ui.add(
            egui::TextEdit::singleline(&mut edited.manifest_public_key_base64)
                .hint_text("32-byte public key; base64")
                .desired_width(ui.available_width())
                .font(egui::TextStyle::Monospace),
        )
        .on_hover_text(
            "Existing single-key installs keep this mapping during rotation. Non-secret verification keys only; never put bearer tokens or private keys here.",
        );

        ui.label("Trusted origin rotation keys (key_id:base64)");
        let mut remove_rotation_key = None;
        for (index, entry) in edited.trusted_origin_signing_keys.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(entry)
                        .hint_text("rw-origin-v2:base64-public-key")
                        .desired_width((ui.available_width() - 70.0).max(120.0))
                        .font(egui::TextStyle::Monospace),
                );
                if ui.small_button("Remove").clicked() {
                    remove_rotation_key = Some(index);
                }
            });
        }
        if let Some(index) = remove_rotation_key {
            edited.trusted_origin_signing_keys.remove(index);
        }
        let legacy_key_count = usize::from(!edited.manifest_public_key_base64.trim().is_empty());
        if ui
            .add_enabled(
                edited.trusted_origin_signing_keys.len() + legacy_key_count
                    < settings::MAX_COMMUNITY_ORIGIN_SIGNING_KEYS,
                egui::Button::new("Add rotation key"),
            )
            .clicked()
        {
            edited.trusted_origin_signing_keys.push(String::new());
        }
        ui.weak(
            "Up to 8 explicitly pinned keys may overlap during rotation. Duplicate IDs or public keys, malformed entries, and unknown signing IDs fail closed; BowEcho never trusts a key advertised by a server.",
        );

        let origin_valid = settings::public_https_base_url_is_valid(&edited.origin_url);
        let r2_valid = settings::optional_public_https_base_url_is_valid(&edited.r2_hot_object_url);
        let key_valid = settings::community_origin_keyring_is_valid(
            &edited.manifest_public_key_base64,
            &edited.trusted_origin_signing_keys,
        );
        let ready = origin_valid && r2_valid && key_valid;
        if edited.enabled && !ready {
            edited.enabled = false;
        }
        let ok = egui::Color32::from_rgb(108, 218, 142);
        let warning = egui::Color32::from_rgb(244, 194, 92);
        if origin_valid {
            ui.colored_label(ok, "Origin URL is valid HTTPS.");
        } else {
            ui.colored_label(warning, "A public HTTPS origin URL is required.");
        }
        if !r2_valid {
            ui.colored_label(warning, "R2 URL must be blank or a public HTTPS base URL.");
        }
        if key_valid {
            ui.colored_label(ok, "Origin signing keyring is valid and explicitly pinned.");
        } else {
            ui.colored_label(
                warning,
                "Configure 1–8 unique key_id:Ed25519 public-key mappings (the legacy field maps to rw-origin-v1).",
            );
        }
        egui::CollapsingHeader::new("Origin account · optional")
            .id_salt("community_cache_origin_account")
            .default_open(false)
            .show(ui, |ui| {
                ui.weak(
                    "If the origin requires a bearer token, BowEcho stores it only in this device's operating-system credential vault.",
                );
                let token_id = egui::Id::new("community_cache_origin_token_draft");
                let status_id = egui::Id::new("community_cache_origin_token_status");
                let mut token: String = ui
                    .ctx()
                    .data(|data| data.get_temp(token_id))
                    .unwrap_or_default();
                let mut token_status: Option<(bool, String)> =
                    ui.ctx().data(|data| data.get_temp(status_id));
                ui.add(
                    egui::TextEdit::singleline(&mut token)
                        .hint_text("bearer token")
                        .password(true)
                        .desired_width(ui.available_width()),
                );
                ui.horizontal_wrapped(|ui| {
                    let can_save = !token.trim().is_empty();
                    if ui
                        .add_enabled(can_save, egui::Button::new("Save securely"))
                        .clicked()
                    {
                        let result = crate::community_credentials::CommunityOriginCredentials::new(
                            &token,
                        )
                        .and_then(|credentials| {
                            crate::community_credentials::save_credentials(&credentials)
                        });
                        match result {
                            Ok(()) => {
                                token.clear();
                                token_status = Some((true, "Origin token saved securely.".to_owned()));
                            }
                            Err(error) => token_status = Some((false, error.to_string())),
                        }
                    }
                    if ui.button("Check vault").clicked() {
                        match crate::community_credentials::load_credentials() {
                            Ok(Some(_)) => {
                                token_status =
                                    Some((true, "An origin token is stored securely.".to_owned()));
                            }
                            Ok(None) => {
                                token_status = Some((true, "No origin token is stored.".to_owned()));
                            }
                            Err(error) => token_status = Some((false, error.to_string())),
                        }
                    }
                    if ui.button("Forget").clicked() {
                        match crate::community_credentials::delete_credentials() {
                            Ok(true) => {
                                token.clear();
                                token_status =
                                    Some((true, "Stored origin token removed.".to_owned()));
                            }
                            Ok(false) => {
                                token_status = Some((true, "No origin token was stored.".to_owned()));
                            }
                            Err(error) => token_status = Some((false, error.to_string())),
                        }
                    }
                });
                if let Some((success, message)) = &token_status {
                    ui.colored_label(if *success { ok } else { warning }, message);
                }
                ui.ctx().data_mut(|data| {
                    data.insert_temp(token_id, token);
                    data.insert_temp(status_id, token_status);
                });
            });
        if ui
            .add_enabled(
                ready,
                egui::Checkbox::new(&mut edited.enabled, "Enable Community Cache"),
            )
            .on_disabled_hover_text("Configure a valid HTTPS origin and pinned public key first")
            .changed()
        {
            ui.ctx().request_repaint();
        }

        ui.separator();
        ui.label(egui::RichText::new("Download/cache allowlist").strong());
        ui.checkbox(&mut edited.soundings_profiles, "Soundings / profiles");
        ui.checkbox(&mut edited.point_series, "Point series");
        ui.checkbox(&mut edited.native_windows_tiles, "Native windows / tiles");
        ui.checkbox(&mut edited.temporal_diurnal, "Temporal / diurnal products");
        ui.checkbox(
            &mut edited.explicit_case_rooms,
            "Published case rooms and artifacts",
        );
        ui.weak(
            "These are the HTTPS cache categories. Arbitrary files and private directories are never eligible. Private WRF/ArWen publication requires a separate deliberate owner action and confirmed redistribution rights.",
        );

        ui.separator();
        ui.label(egui::RichText::new("Resource limits").strong());
        panel_kit::row(ui, "Disk allowance", |ui| {
            ui.add(
                egui::DragValue::new(&mut edited.disk_allowance_gib)
                    .range(1..=2_048)
                    .suffix(" GiB"),
            );
        });
        panel_kit::row(ui, "Download cap", |ui| {
            ui.add(
                egui::DragValue::new(&mut edited.download_cap_mib_per_hour)
                    .range(1..=1_048_576)
                    .suffix(" MiB/hour"),
            );
        });
        panel_kit::row(ui, "Relay upload cap", |ui| {
            ui.add(
                egui::DragValue::new(&mut edited.upload_cap_mib_per_hour)
                    .range(1..=1_048_576)
                    .suffix(" MiB/hour"),
            )
            .on_hover_text(
                "Applies only to explicit cold-object seeding; operational HTTPS never uploads",
            );
        });
        panel_kit::row(ui, "Monthly transfer", |ui| {
            ui.add(
                egui::DragValue::new(&mut edited.monthly_transfer_cap_gib)
                    .range(1..=10_000)
                    .suffix(" GiB"),
            );
        });
        panel_kit::row(ui, "Concurrent downloads", |ui| {
            ui.add(egui::DragValue::new(&mut edited.max_concurrent_downloads).range(1..=16));
        });
        ui.weak(
            "Disk, hourly, monthly, and concurrency limits apply fail-closed. Relay service quotas, cost thresholds, and its global kill switch can impose stricter limits.",
        );
        ui.checkbox(
            &mut edited.pause_sharing_on_metered_networks,
            "Pause relay seeding on metered networks",
        )
        .on_hover_text(
            "On by default. This blocks relay uploads/seeding only; it never pauses operational HTTPS downloads.",
        );

        ui.separator();
        ui.label(egui::RichText::new("Cold historical relay · advanced").strong());
        ui.weak(
            "Initial transport is intentionally limited to origin-signed profiles and point series no larger than 64 KiB. Native/geographic windows, temporal products, and case artifacts use archival HTTPS fallback until a bounded sliding-window transport passes release gates.",
        );
        ui.label("Pinned relay credential key (Ed25519, base64)");
        ui.add(
            egui::TextEdit::singleline(&mut edited.relay_public_key_base64)
                .hint_text("relay public key; never a private key")
                .desired_width(ui.available_width())
                .font(egui::TextStyle::Monospace),
        );
        ui.label("Trusted relay rotation keys (key_id:base64)");
        let mut remove_relay_key = None;
        for (index, entry) in edited.trusted_relay_signing_keys.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(entry)
                        .hint_text("rw-relay-v2:base64-public-key")
                        .desired_width((ui.available_width() - 70.0).max(120.0))
                        .font(egui::TextStyle::Monospace),
                );
                if ui.small_button("Remove").clicked() {
                    remove_relay_key = Some(index);
                }
            });
        }
        if let Some(index) = remove_relay_key {
            edited.trusted_relay_signing_keys.remove(index);
        }
        let legacy_relay_key_count = usize::from(!edited.relay_public_key_base64.trim().is_empty());
        if ui
            .add_enabled(
                edited.trusted_relay_signing_keys.len() + legacy_relay_key_count
                    < settings::MAX_COMMUNITY_RELAY_SIGNING_KEYS,
                egui::Button::new("Add relay rotation key"),
            )
            .clicked()
        {
            edited.trusted_relay_signing_keys.push(String::new());
        }
        ui.weak(
            "Relay verification keys are local trust pins. Rotation may overlap up to 8 keys; removing a pin immediately removes objects and credentials signed only by that key from eligibility.",
        );
        egui::CollapsingHeader::new("Audited TURN provider allocation ranges")
            .id_salt("community_cache_turn_ranges")
            .default_open(false)
            .show(ui, |ui| {
                ui.weak(
                    "Deployment-managed CIDRs only. Empty or malformed ranges reject all routes; host, server-reflexive, LAN, and direct candidates are never accepted.",
                );
                let mut remove_range = None;
                for (index, range) in edited
                    .relay_provider_allocation_cidrs
                    .iter_mut()
                    .enumerate()
                {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(range)
                                .hint_text("provider allocation CIDR")
                                .desired_width((ui.available_width() - 70.0).max(120.0))
                                .font(egui::TextStyle::Monospace),
                        );
                        if ui.small_button("Remove").clicked() {
                            remove_range = Some(index);
                        }
                    });
                }
                if let Some(index) = remove_range {
                    edited.relay_provider_allocation_cidrs.remove(index);
                }
                if edited.relay_provider_allocation_cidrs.len() < 128
                    && ui.button("Add provider range").clicked()
                {
                    edited.relay_provider_allocation_cidrs.push(String::new());
                }
            });
        let mut relay_candidate = edited.clone();
        relay_candidate.historical_relay_enabled = true;
        let relay_prerequisites = relay_candidate.historical_relay_ready();
        if edited.historical_relay_enabled && !relay_prerequisites {
            edited.historical_relay_enabled = false;
            edited.historical_relay_seeding_enabled = false;
        }
        ui.add_enabled(
            relay_prerequisites,
            egui::Checkbox::new(
                &mut edited.historical_relay_enabled,
                "Enable explicit cold-historical relay retrieval",
            ),
        )
        .on_disabled_hover_text(
            "Enable Community Cache and configure a pinned relay key plus audited provider ranges first",
        );
        if !edited.historical_relay_enabled {
            edited.historical_relay_seeding_enabled = false;
        }
        ui.add_enabled(
            edited.historical_relay_enabled,
            egui::Checkbox::new(
                &mut edited.historical_relay_seeding_enabled,
                "Seed already verified eligible cold objects",
            ),
        );
        let unmetered_changed = ui
            .add_enabled(
                edited.historical_relay_seeding_enabled,
                egui::Checkbox::new(
                    &mut self.community_relay_network_unmetered_confirmed,
                    "I confirm this session's network is unmetered",
                ),
            )
            .on_hover_text(
                "Session-only and off again after restart. Unknown networks are conservatively treated as metered, so seeding remains paused.",
            )
            .changed();
        if edited.historical_relay_enabled && ui.small_button("Stop relay now").clicked() {
            edited.historical_relay_enabled = false;
            edited.historical_relay_seeding_enabled = false;
            self.community_relay_network_unmetered_confirmed = false;
            if let Some(runtime) = &self.community_relay_runtime {
                runtime.reconfigure(&edited, settings::community_cache_dir(), false);
            }
        }
        if let Some(runtime) = &self.community_relay_runtime {
            let status = runtime.status();
            ui.separator();
            ui.label(egui::RichText::new("Runtime").strong());
            ui.label(status.summary());
            ui.weak(format!(
                "Verified eligible: {}  ·  advertised: {}  ·  recovered: {}  ·  archival fallbacks: {}",
                status.verified_seed_objects,
                status.advertised_objects,
                status.recovered_objects,
                status.archival_fallbacks,
            ));
            if let Some(code) = status.last_failure_code {
                ui.colored_label(warning, format!("Last closed failure: {code}"));
            }
        }
        ui.weak(
            "The operator must separately verify current provider pricing and enable capacity/security gates. Cloudflare's published Realtime terms currently describe a shared 1,000 GB/month SFU+TURN allowance, then $0.05/GB edge-to-client including TURN overhead; terms and account treatment can change, so this is not a permanent guarantee.",
        );

        ui.separator();
        ui.label(egui::RichText::new("Privacy").strong());
        ui.weak(
            "Other users never see your IP address. The service operator sees the IP and request/transfer metadata necessary to operate, secure, and limit the service.",
        );
        ui.weak(
            "Cold community transfers are TURN-only and end-to-end encrypted. Other users never receive your address, provider allocation, or credentials; there is no STUN, ICE, LAN discovery, or direct fallback.",
        );

        let settings_changed = edited != original;
        if settings_changed {
            self.app_settings.community_cache = edited;
            let client = crate::community_cache::CommunityCacheClient::from_settings(
                &self.app_settings.community_cache,
                settings::community_cache_dir(),
            )
            .ok();
            if let Some(dock) = self.model_dock.as_mut() {
                dock.set_community_cache_client(client);
                dock.set_federation_settings(
                    &self.app_settings.community_cache,
                    &self.app_settings.federation,
                );
            }
            self.mark_app_settings_dirty();
        }
        if (settings_changed || unmetered_changed)
            && let Some(runtime) = &self.community_relay_runtime
        {
            runtime.reconfigure(
                &self.app_settings.community_cache,
                settings::community_cache_dir(),
                self.community_relay_network_unmetered_confirmed,
            );
        }
    }

    fn federation_settings_section(&mut self, ui: &mut egui::Ui) {
        let original = self.app_settings.federation.clone();
        let mut edited = original.clone();
        let ok = egui::Color32::from_rgb(108, 218, 142);
        let warning = egui::Color32::from_rgb(244, 194, 92);

        ui.weak(
            "BowEcho talks only to the configured authoritative Rusty Weather / Hetzner HTTPS origin. After a normal local/R2/origin miss, that authority may fetch from an independently pinned university or lab and re-sign the exact object.",
        );
        ui.weak(
            "Institutional addresses are deliberately public. BowEcho never contacts them, never forwards the authority bearer, and never puts ordinary Community Cache participants in this catalog.",
        );
        ui.checkbox(
            &mut edited.enabled,
            "Enable authority-mediated public-origin failover",
        );
        ui.separator();

        ui.label(egui::RichText::new("Authority catalog signing pins").strong());
        let mut remove_catalog_key = None;
        for (index, key) in edited.catalog_signing_keys.iter_mut().enumerate() {
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut key.key_id)
                        .hint_text("catalog-key-id")
                        .desired_width(150.0)
                        .font(egui::TextStyle::Monospace),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut key.public_key_base64)
                        .hint_text("Ed25519 public key; base64")
                        .desired_width((ui.available_width() - 70.0).max(180.0))
                        .font(egui::TextStyle::Monospace),
                );
                if ui.small_button("Remove").clicked() {
                    remove_catalog_key = Some(index);
                }
            });
        }
        if let Some(index) = remove_catalog_key {
            edited.catalog_signing_keys.remove(index);
        }
        if edited.catalog_signing_keys.len() < settings::MAX_FEDERATION_CATALOG_SIGNING_KEYS
            && ui.button("Add catalog key pin").clicked()
        {
            edited
                .catalog_signing_keys
                .push(settings::FederationPinnedKeySettings::default());
        }

        ui.separator();
        ui.label(egui::RichText::new("Approved public origins").strong());
        ui.weak(
            "Each origin ID and its descriptor keys are pinned independently from the authority catalog. A catalog cannot bootstrap a new institution by itself.",
        );
        let mut remove_origin = None;
        for (origin_index, origin) in edited.approved_origins.iter_mut().enumerate() {
            ui.group(|ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Origin ID");
                    ui.add(
                        egui::TextEdit::singleline(&mut origin.origin_id)
                            .hint_text("university-or-lab-id")
                            .desired_width(220.0)
                            .font(egui::TextStyle::Monospace),
                    );
                    if ui.small_button("Remove origin").clicked() {
                        remove_origin = Some(origin_index);
                    }
                });
                let mut remove_key = None;
                for (key_index, key) in origin.descriptor_signing_keys.iter_mut().enumerate() {
                    ui.horizontal_wrapped(|ui| {
                        ui.weak("Descriptor key");
                        ui.add(
                            egui::TextEdit::singleline(&mut key.key_id)
                                .hint_text("descriptor-key-id")
                                .desired_width(150.0)
                                .font(egui::TextStyle::Monospace),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut key.public_key_base64)
                                .hint_text("Ed25519 public key; base64")
                                .desired_width((ui.available_width() - 70.0).max(180.0))
                                .font(egui::TextStyle::Monospace),
                        );
                        if ui.small_button("Remove").clicked() {
                            remove_key = Some(key_index);
                        }
                    });
                }
                if let Some(index) = remove_key {
                    origin.descriptor_signing_keys.remove(index);
                }
                if origin.descriptor_signing_keys.len()
                    < settings::MAX_FEDERATION_DESCRIPTOR_KEYS_PER_ORIGIN
                    && ui.small_button("Add descriptor key pin").clicked()
                {
                    origin
                        .descriptor_signing_keys
                        .push(settings::FederationPinnedKeySettings::default());
                }
            });
        }
        if let Some(index) = remove_origin {
            edited.approved_origins.remove(index);
        }
        if edited.approved_origins.len() < settings::MAX_FEDERATION_APPROVED_ORIGINS
            && ui.button("Add approved public origin").clicked()
        {
            edited
                .approved_origins
                .push(settings::FederationApprovedOriginSettings {
                    origin_id: String::new(),
                    descriptor_signing_keys: vec![settings::FederationPinnedKeySettings::default()],
                });
        }

        ui.separator();
        egui::CollapsingHeader::new("Revocations")
            .id_salt("federation_revocations")
            .default_open(false)
            .show(ui, |ui| {
                ui.weak(
                    "Revoked IDs remain explicit and fail closed. Remove a revocation only as a deliberate trust-policy change.",
                );
                ui.label("Revoked origin IDs");
                let mut remove = None;
                for (index, value) in edited.revoked_origin_ids.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(value)
                                .hint_text("revoked-origin-id")
                                .desired_width((ui.available_width() - 70.0).max(180.0))
                                .font(egui::TextStyle::Monospace),
                        );
                        if ui.small_button("Remove").clicked() {
                            remove = Some(index);
                        }
                    });
                }
                if let Some(index) = remove {
                    edited.revoked_origin_ids.remove(index);
                }
                if edited.revoked_origin_ids.len() < settings::MAX_FEDERATION_REVOCATIONS
                    && ui.small_button("Add revoked origin").clicked()
                {
                    edited.revoked_origin_ids.push(String::new());
                }

                ui.label("Revoked signing key IDs");
                let mut remove = None;
                for (index, value) in edited.revoked_key_ids.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(value)
                                .hint_text("revoked-key-id")
                                .desired_width((ui.available_width() - 70.0).max(180.0))
                                .font(egui::TextStyle::Monospace),
                        );
                        if ui.small_button("Remove").clicked() {
                            remove = Some(index);
                        }
                    });
                }
                if let Some(index) = remove {
                    edited.revoked_key_ids.remove(index);
                }
                if edited.revoked_key_ids.len() < settings::MAX_FEDERATION_REVOCATIONS
                    && ui.small_button("Add revoked key").clicked()
                {
                    edited.revoked_key_ids.push(String::new());
                }
            });

        if edited.trust_ready() {
            ui.colored_label(ok, "Federation trust policy is bounded and fully pinned.");
        } else {
            ui.colored_label(
                warning,
                "Trust policy is incomplete, duplicated, revoked, malformed, or over its bound; federation remains off.",
            );
        }
        if !self.app_settings.community_cache.phase1_active() {
            ui.colored_label(
                warning,
                "Configure and enable the authoritative Community Cache HTTPS path first.",
            );
        }

        if edited != original {
            self.app_settings.federation = edited;
            if let Some(dock) = self.model_dock.as_mut() {
                dock.set_federation_settings(
                    &self.app_settings.community_cache,
                    &self.app_settings.federation,
                );
            }
            self.mark_app_settings_dirty();
        }

        ui.separator();
        if let Some(dock) = self.model_dock.as_mut() {
            dock.federation_discovery_ui(ui);
        } else {
            ui.weak("Open the Model workspace to start signed public-origin discovery.");
        }
    }

    fn model_settings_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if ui
            .checkbox(&mut self.model_enabled, "Model data")
            .on_hover_text(
                "Master switch: off = pure radar app (no model dock, layer, hover value, or Alt-click soundings)",
            )
            .changed()
        {
            if self.model_enabled {
                let store = settings::model_store_dir();
                if store
                    .read_dir()
                    .map(|mut entries| entries.next().is_some())
                    .unwrap_or(false)
                {
                    self.model_dock = Some(self.new_model_data_dock(ctx, store));
                }
            }
            ctx.request_repaint();
        }
        ui.horizontal(|ui| {
            ui.label("Keep runs");
            let mut keep = self.model_keep_runs;
            if ui
                .add(egui::DragValue::new(&mut keep).range(0..=24).speed(0.1))
                .on_hover_text(
                    "Model store retention: newest N runs auto-kept, older deleted after each fetch and at startup (0 = unlimited). Default 2 keeps SSD use ~1.5 GB.",
                )
                .changed()
            {
                self.model_keep_runs = keep;
                self.app_settings.model_keep_runs = keep;
                self.mark_app_settings_dirty();
            }
        });
        ui.weak(format!("Store: {}", settings::model_store_dir().display()))
            .on_hover_text("Model store location (rusty-weather rw-store layout)");
    }
}
