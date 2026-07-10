//! Hazard panel UI and hazard map-paint methods moved verbatim out of
//! `main.rs` (v0.29.4 decomposition, queue item #3). Hazard types, statics,
//! and constants stay in `main.rs` (pure geometry in `hazard_geom.rs`);
//! this module reaches them via `crate::`.

use crate::*;

impl ViewerApp {
    pub(crate) fn hazard_panel(&mut self, ui: &mut egui::Ui) {
        // Wrapped, not a kit row: four toggles outgrow the control column
        // at 320 pt and wrapping keeps them one visual family.
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.hazards_visible, "Show")
                .on_hover_text("Draw warning polygons on the map (also the Map-tab Warnings row)");
            let mut show_labels = self.app_settings.show_hazard_labels;
            if ui
                .checkbox(&mut show_labels, "Labels")
                .on_hover_text("Draw compact warning labels such as SVR 0653 on the map")
                .changed()
            {
                self.app_settings.show_hazard_labels = show_labels;
                self.mark_app_settings_dirty();
                ui.ctx().request_repaint();
            }
            ui.checkbox(&mut self.hazards_active_only, "Active only")
                .on_hover_text("Hide expired/cancelled alerts");
            ui.checkbox(&mut self.live_hazard_auto_refresh, "Auto-refresh")
                .on_hover_text("Re-fetch active alerts on the live cadence");
        });
        // Family filters as a kit chip grid: selected = shown on the map
        // and in the list; the hidden-family set is the same state the
        // checkboxes used to edit.
        let family_chips = HAZARD_FILTER_FAMILIES
            .iter()
            .map(|&(family, label)| panel_kit::Chip {
                label,
                hotkey: None,
                selected: !self.hidden_hazard_families.contains(family),
                hover: Some(format!("Show {family} alerts on the map and in the list")),
            })
            .collect::<Vec<_>>();
        if let Some(clicked) = panel_kit::chip_grid(ui, &family_chips) {
            let (family, _) = HAZARD_FILTER_FAMILIES[clicked];
            if !self.hidden_hazard_families.remove(family) {
                self.hidden_hazard_families.insert(family.to_owned());
            }
            if self
                .selected_hazard_record()
                .is_some_and(|record| !self.hazard_record_visible(record))
            {
                self.selected_hazard_index = None;
            }
            ui.ctx().request_repaint();
        }
        // The fill slider reads/writes the style registry (override on the
        // styles.json document) so it persists across launches.
        let mut fill_alpha = self.style_registry.hazard_global().fill_alpha as f32;
        let fill_response =
            panel_kit::slider_row(ui, "Fill", &mut fill_alpha, 0.0..=80.0, 0.0, |value| {
                format!("{value:.0}")
            })
            .on_hover_text("Warning-polygon fill opacity (0-80)");
        if fill_response.changed() {
            self.style_settings.hazard_global.fill_alpha = Some(fill_alpha.round() as u8);
            self.rebuild_style_registry();
            ui.ctx().request_repaint();
        }
        if fill_response.drag_stopped() || (fill_response.changed() && !fill_response.dragged()) {
            self.save_styles();
        }
        ui.horizontal(|ui| {
            let loading = self.hazard_receiver.is_some();
            if fixed_action_button(ui, "Refresh Live", 96.0).clicked() && !loading {
                self.refresh_live_hazards_manually(ui.ctx());
            }
            if fixed_action_button(ui, "Clear", 52.0).clicked() {
                self.hazard_overlay_generation = self.hazard_overlay_generation.wrapping_add(1);
                self.hazard_overlay = None;
                self.selected_hazard_index = None;
                self.unacknowledged_hazard_event_ids.clear();
                self.hazard_status = "No hazard polygons loaded".to_owned();
            }
        });

        self.remembered_section(
            ui,
            "severe_current_alerts",
            "Current alerts",
            true,
            |app, ui| {
                let rows = app.visible_hazard_list_rows();
                let total = app
                    .hazard_overlay
                    .as_ref()
                    .map(|overlay| overlay.records.len())
                    .unwrap_or(0);
                if app.hazard_receiver.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("refreshing alerts");
                    });
                }
                if total == 0 {
                    ui.weak("No hazard polygons loaded");
                    return;
                }
                panel_kit::row(ui, "Type", |ui| {
                    let mut filter =
                        HazardListFilter::from_key(&app.app_settings.current_alert_filter);
                    egui::ComboBox::from_id_salt("current_alert_filter")
                        .selected_text(filter.label())
                        .width(104.0)
                        .show_ui(ui, |ui| {
                            for option in HazardListFilter::ALL {
                                ui.selectable_value(&mut filter, option, option.label());
                            }
                        });
                    if filter.key() != app.app_settings.current_alert_filter {
                        app.app_settings.current_alert_filter = filter.key().to_owned();
                        if let Some(family) = filter.family() {
                            app.hidden_hazard_families.remove(family);
                        }
                        app.mark_app_settings_dirty();
                        ui.ctx().request_repaint();
                    }
                });
                panel_kit::row(ui, "Sort", |ui| {
                    let mut sort = HazardListSort::from_key(&app.app_settings.current_alert_sort);
                    egui::ComboBox::from_id_salt("current_alert_sort")
                        .selected_text(sort.label())
                        .width(112.0)
                        .show_ui(ui, |ui| {
                            for option in HazardListSort::ALL {
                                ui.selectable_value(&mut sort, option, option.label());
                            }
                        });
                    if sort.key() != app.app_settings.current_alert_sort {
                        app.app_settings.current_alert_sort = sort.key().to_owned();
                        app.mark_app_settings_dirty();
                    }
                });
                ui.weak(format!("{} shown of {} loaded", rows.len(), total));
                if rows.is_empty() {
                    ui.weak("No alerts match the active filters");
                    return;
                }
                let unacknowledged_count = rows.iter().filter(|row| row.unacknowledged).count();
                if unacknowledged_count > 0 {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(255, 202, 92),
                            format!("{unacknowledged_count} new"),
                        );
                        if fixed_action_button(ui, "Ack all", 58.0).clicked() {
                            app.acknowledge_all_visible_hazards();
                        }
                    });
                }
                // Kit select_row list: fixed-order monospace columns
                // (family code | id | office | until) — the office
                // truncates, the expiry and the NEW flag never do. The
                // NEW flag replaces the old yellow row text (select_row
                // paints one color); the "N new" banner and the map
                // flashes keep carrying the attention state.
                let display_rows = rows
                    .iter()
                    .map(|row| {
                        let record = app
                            .hazard_overlay
                            .as_ref()
                            .and_then(|overlay| overlay.records.get(row.index));
                        let code = hazard_family_menu_label(&row.family);
                        let raw_label = record
                            .map(|record| record.label.clone())
                            .unwrap_or_else(|| row.label.clone());
                        let office = record
                            .map(|record| record.office.clone())
                            .unwrap_or_default();
                        let until = record
                            .and_then(|record| record.valid_end.as_deref())
                            .and_then(hazard_until_hhmmz);
                        (row, code, raw_label, office, until)
                    })
                    .collect::<Vec<_>>();
                let mut focus_index = None;
                fixed_height_scroll(
                    ui,
                    "hazard_current_alerts_list",
                    HAZARD_LIST_SCROLL_HEIGHT,
                    |ui| {
                        let char_width = ui
                            .fonts(|fonts| fonts.glyph_width(&egui::FontId::monospace(12.0), '0'))
                            .max(1.0);
                        let max_chars =
                            (((ui.available_width() - 12.0) / char_width).floor() as usize).max(8);
                        for (row, code, raw_label, office, until) in &display_rows {
                            let text = hazard_alert_row_text(
                                code,
                                raw_label,
                                office,
                                until.as_deref(),
                                row.unacknowledged,
                                max_chars,
                            );
                            let response = panel_kit::select_row(
                                ui,
                                row.selected,
                                true,
                                &text,
                                Some(row.hover.as_str()),
                            );
                            if response.clicked() {
                                focus_index = Some(row.index);
                            }
                        }
                    },
                );
                if let Some(index) = focus_index
                    && app.focus_hazard_record(index, ui.ctx())
                {
                    ui.ctx().request_repaint();
                }
            },
        );

        if let Some(record) = self.selected_hazard_record() {
            ui.add_space(6.0);
            let detail_lines = hazard_record_detail_lines(record);
            fixed_height_scroll(
                ui,
                "hazard_detail_text",
                HAZARD_DETAIL_SCROLL_HEIGHT,
                |ui| {
                    for line in &detail_lines {
                        wrapped_label(ui, line);
                    }
                },
            );
        }

        let summary_lines = self.hazard_summary_lines();
        fixed_height_scroll(
            ui,
            "hazard_summary_text",
            HAZARD_SUMMARY_SCROLL_HEIGHT,
            |ui| {
                for line in &summary_lines {
                    wrapped_label(ui, line);
                }
            },
        );

        // SPC OUTLOOKS — config consolidated here from the layer rail (spec
        // §1 SEVERE table); the rail's SPC rows' ⚙ jumps to this section.
        self.remembered_section(ui, "severe_spc_outlooks", "Outlooks", true, |app, ui| {
            let mut changed = false;
            let mut estofex_on = app
                .spc_outlooks_enabled
                .iter()
                .any(|k| k == spc_layers::ESTOFEX_OUTLOOK_KIND);
            let estofex_count = app
                .spc_data
                .estofex_issues
                .iter()
                .map(|issue| issue.polygons.len())
                .sum::<usize>();
            let displayed_time = app.displayed_timeline_time_utc().unwrap_or_else(Utc::now);
            let selected_estofex_issue = spc_layers::selected_estofex_issue(
                &app.spc_data.estofex_issues,
                app.estofex_issue_id.as_deref(),
                displayed_time,
            )
            .map(|issue| {
                (
                    issue.id.clone(),
                    spc_layers::estofex_issue_label(issue),
                    issue.polygons.len(),
                    spc_layers::estofex_issue_valid_at(issue, displayed_time),
                )
            });
            panel_kit::row(ui, "Day", |ui| {
                egui::ComboBox::from_id_salt("severe_spc_day")
                    .selected_text(format!("D{}", app.spc_day))
                    .width(64.0)
                    .show_ui(ui, |ui| {
                        for d in 1..=3u8 {
                            if ui
                                .selectable_value(&mut app.spc_day, d, format!("Day {d}"))
                                .changed()
                            {
                                changed = true;
                            }
                        }
                    })
                    .response
                    .on_hover_text(
                        "Outlook day (archive-aware: an archive loop shows THAT day's outlook)",
                    );
                if app.spc_rx.is_some() {
                    ui.spinner();
                }
            });
            // Outlook kinds + the Reports / ESTOFEX toggles as one kit chip
            // grid — same enable state the checkboxes used to edit.
            let kinds = spc_layers::outlook_kind_options(app.spc_day);
            let mut outlook_chips = kinds
                .iter()
                .map(|&(slug, label)| {
                    let on = if app.spc_day == 3 && slug == "prob" {
                        app.spc_outlooks_enabled
                            .iter()
                            .any(|k| matches!(k.as_str(), "prob" | "torn" | "wind" | "hail"))
                    } else {
                        app.spc_outlooks_enabled.iter().any(|k| k.as_str() == slug)
                    };
                    panel_kit::Chip {
                        label,
                        hotkey: None,
                        selected: on,
                        hover: Some("Outlook kind — drawn in SPC's own colors".to_owned()),
                    }
                })
                .collect::<Vec<_>>();
            let reports_chip = outlook_chips.len();
            outlook_chips.push(panel_kit::Chip {
                label: "Reports",
                hotkey: None,
                selected: app.spc_reports_enabled,
                hover: Some(
                    "Today's filtered storm reports (tornado / wind / hail) — same state as the Map-tab row"
                        .to_owned(),
                ),
            });
            let estofex_chip = outlook_chips.len();
            outlook_chips.push(panel_kit::Chip {
                label: "ESTOFEX Europe",
                hotkey: None,
                selected: estofex_on,
                hover: Some(
                    "ESTOFEX European Storm Forecast Experiment outlooks, with issue selection separate from SPC day"
                        .to_owned(),
                ),
            });
            if let Some(clicked) = panel_kit::chip_grid(ui, &outlook_chips) {
                if let Some((slug, _)) = kinds.get(clicked) {
                    let was_on = outlook_chips[clicked].selected;
                    if app.spc_day == 3 && *slug == "prob" {
                        app.spc_outlooks_enabled
                            .retain(|k| !matches!(k.as_str(), "prob" | "torn" | "wind" | "hail"));
                    } else {
                        app.spc_outlooks_enabled.retain(|k| k != slug);
                    }
                    if !was_on {
                        app.spc_outlooks_enabled.push((*slug).to_owned());
                    }
                    changed = true;
                } else if clicked == reports_chip {
                    app.spc_reports_enabled = !app.spc_reports_enabled;
                    changed = true;
                } else if clicked == estofex_chip {
                    estofex_on = !estofex_on;
                    if estofex_on {
                        app.spc_outlooks_enabled
                            .push(spc_layers::ESTOFEX_OUTLOOK_KIND.to_owned());
                    } else {
                        app.spc_outlooks_enabled
                            .retain(|k| k != spc_layers::ESTOFEX_OUTLOOK_KIND);
                        app.estofex_issue_id = None;
                    }
                    changed = true;
                }
            }
            if fixed_action_button(ui, "Center Europe", 108.0)
                .on_hover_text("Jump to a Europe overview so ESTOFEX polygons are visible")
                .clicked()
            {
                app.center_map_on(50.5, 12.0);
                app.map_scale = 22.0;
                app.status = "Centered map on Europe for ESTOFEX".to_owned();
                ui.ctx().request_repaint();
            }
            if estofex_on {
                panel_kit::row(ui, "ESTOFEX issue", |ui| {
                    let selected_text = selected_estofex_issue
                        .as_ref()
                        .map(|(_, label, _, valid)| {
                            if *valid || app.estofex_issue_id.is_some() {
                                label.clone()
                            } else {
                                "Auto - no valid issue".to_owned()
                            }
                        })
                        .unwrap_or_else(|| {
                            if app.estofex_issue_id.is_some() {
                                "Missing selected issue".to_owned()
                            } else {
                                "Auto - valid at displayed time".to_owned()
                            }
                        });
                    egui::ComboBox::from_id_salt("severe_estofex_issue")
                        .selected_text(selected_text)
                        .width(ui.available_width().clamp(120.0, 260.0))
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_value(
                                    &mut app.estofex_issue_id,
                                    None,
                                    "Auto - valid at displayed time",
                                )
                                .changed()
                            {
                                changed = true;
                            }
                            for issue in &app.spc_data.estofex_issues {
                                if ui
                                    .selectable_value(
                                        &mut app.estofex_issue_id,
                                        Some(issue.id.clone()),
                                        spc_layers::estofex_issue_label(issue),
                                    )
                                    .changed()
                                {
                                    changed = true;
                                }
                            }
                        })
                        .response
                        .on_hover_text(
                            "Auto chooses the newest issue valid at the displayed radar time and never shows an update before it was issued.",
                        );
                });
                let status = if app.spc_rx.is_some() {
                    "ESTOFEX: fetching issue list".to_owned()
                } else if let Some((_, label, area_count, valid)) = &selected_estofex_issue {
                    let stale = if *valid { "" } else { " - stale at displayed time" };
                    format!("ESTOFEX: {area_count} areas, {label}{stale}")
                } else if estofex_count > 0 {
                    format!(
                        "ESTOFEX: {estofex_count} areas cached, none valid at displayed time"
                    )
                } else if app.spc_data.fetched_at.is_some() {
                    "ESTOFEX: no issues returned by the live feed".to_owned()
                } else {
                    "ESTOFEX: waiting for the next issue fetch".to_owned()
                };
                ui.weak(status);
            }
            if changed {
                if !app.spc_outlooks_enabled.is_empty() {
                    app.spc_kinds_memory = app.spc_outlooks_enabled.clone();
                }
                app.spc_data.fetched_at = None; // force refetch
                app.status = if app
                    .spc_outlooks_enabled
                    .iter()
                    .any(|k| k == spc_layers::ESTOFEX_OUTLOOK_KIND)
                {
                    "Fetching outlooks with ESTOFEX".to_owned()
                } else {
                    "Fetching SPC outlooks".to_owned()
                };
                app.save_overlay_defaults();
                ui.ctx().request_repaint();
            }
        });
        self.remembered_section(
            ui,
            "severe_warning_feed",
            "Warning feed",
            false,
            |app, ui| {
                panel_kit::row(ui, "Auto-refresh every", |ui| {
                    let mut secs = app
                        .app_settings
                        .warning_refresh_seconds
                        .max(MIN_LIVE_HAZARD_REFRESH_SECONDS);
                    if ui
                        .add(
                            egui::DragValue::new(&mut secs)
                                .range(MIN_LIVE_HAZARD_REFRESH_SECONDS..=600)
                                .suffix(" s"),
                        )
                        .on_hover_text(
                            "How often BowEcho re-fetches live warnings (NWS active alerts plus any \
                             custom feed). NWS guidance is 30 s; a fast local or relay feed can poll \
                             down to 5 s.",
                        )
                        .changed()
                    {
                        app.app_settings.warning_refresh_seconds =
                            secs.max(MIN_LIVE_HAZARD_REFRESH_SECONDS);
                        app.mark_app_settings_dirty();
                        ui.ctx().request_repaint();
                    }
                });
                ui.label("Custom provider (poll URL)").on_hover_text(
                "Optional http(s) URL BowEcho polls alongside NWS active alerts and merges into \
                 the warnings layer. Accepts the NWS CAP/GeoJSON alert FeatureCollection (same \
                 shape as api.weather.gov/alerts/active) or the NWS text/VTEC + lat/lon polygon \
                 format.",
            );
                // The URL input truncates its content instead of forcing the
                // panel wider (320 pt rule).
                let response = ui.add(
                    egui::TextEdit::singleline(&mut app.app_settings.warning_provider_url)
                        .desired_width(ui.available_width())
                        .hint_text("https://host/warnings.geojson"),
                );
                if response.lost_focus() {
                    app.mark_app_settings_dirty();
                }
                if !app.app_settings.warning_provider_url.trim().is_empty()
                    && custom_warning_provider_url(&app.app_settings.warning_provider_url).is_none()
                {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 170, 80),
                        "Provider URL must start with http:// or https://",
                    );
                }
            },
        );
        self.remembered_section(ui, "severe_local_file", "Local file", false, |app, ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.hazard_path_text)
                    .desired_width(ui.available_width())
                    .hint_text("Path"),
            );
            let loading = app.hazard_receiver.is_some();
            if fixed_action_button(ui, "Load Path", 82.0).clicked() && !loading {
                app.load_local_hazards(ui.ctx());
            }
        });
    }

    fn hazard_summary_lines(&self) -> Vec<String> {
        let mut lines = vec![self.hazard_status.clone()];
        if let Some(overlay) = &self.hazard_overlay {
            lines.push(format!(
                "{} scanned, {} parsed, {} polygons",
                overlay.scanned_items, overlay.parsed_items, overlay.polygon_records
            ));
            lines.push(overlay.source_label.clone());
            if overlay.error_count > 0 {
                let issue_label = if overlay.error_count == 1 {
                    "source issue"
                } else {
                    "source issues"
                };
                lines.push(format!("{} {issue_label}", overlay.error_count));
            }
            if let Some(query_time_utc) = &overlay.query_time_utc {
                lines.push(format!("At {query_time_utc}"));
            }
        }
        lines
    }

    pub(crate) fn visible_hazard_list_rows(&self) -> Vec<HazardListRow> {
        let Some(overlay) = &self.hazard_overlay else {
            return Vec::new();
        };
        let filter = HazardListFilter::from_key(&self.app_settings.current_alert_filter);
        let mut rows = overlay
            .records
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                self.hazard_record_visible(record) && hazard_points_renderable(&record.points)
            })
            .filter(|(_, record)| filter.matches_record(record))
            .map(|(index, record)| self.hazard_list_row(index, record))
            .collect::<Vec<_>>();
        sort_hazard_list_rows(
            &mut rows,
            &overlay.records,
            HazardListSort::from_key(&self.app_settings.current_alert_sort),
        );
        rows
    }

    fn hazard_list_row(&self, index: usize, record: &HazardRecord) -> HazardListRow {
        HazardListRow {
            index,
            label: hazard_record_list_label(record),
            family: record.event_family.clone(),
            hover: hazard_record_list_hover(record),
            selected: self.selected_hazard_index == Some(index),
            unacknowledged: self
                .unacknowledged_hazard_event_ids
                .contains(&record.event_id),
        }
    }

    pub(crate) fn unacknowledged_hazard_menu_groups(&self) -> Vec<(String, Vec<HazardListRow>)> {
        let Some(overlay) = &self.hazard_overlay else {
            return Vec::new();
        };
        let mut rows = overlay
            .records
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                self.unacknowledged_hazard_event_ids
                    .contains(&record.event_id)
                    && hazard_record_should_latch_attention(record)
            })
            .map(|(index, record)| self.hazard_list_row(index, record))
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            hazard_family_order(&left.family)
                .cmp(&hazard_family_order(&right.family))
                .then_with(|| left.label.cmp(&right.label))
        });

        let mut groups: Vec<(String, Vec<HazardListRow>)> = Vec::new();
        for row in rows {
            let label = hazard_family_menu_label(&row.family);
            if let Some((_, group_rows)) = groups.iter_mut().find(|(family, _)| family == &label) {
                group_rows.push(row);
            } else {
                groups.push((label, vec![row]));
            }
        }
        groups
    }

    pub(crate) fn focus_hazard_record(&mut self, index: usize, ctx: &egui::Context) -> bool {
        let Some((bbox, label)) = self
            .hazard_overlay
            .as_ref()
            .and_then(|overlay| overlay.records.get(index))
            .and_then(|record| {
                (self.hazard_record_visible(record) && hazard_points_renderable(&record.points))
                    .then(|| (record.bbox, record.label.clone()))
            })
        else {
            return false;
        };
        let (lat, lon, scale) = hazard_focus_view(bbox);
        self.select_hazard_index(index);
        self.center_map_on(lat, lon);
        self.map_scale = scale;
        self.clamp_map_center();
        let radar_label =
            self.best_radar_candidates(lat, lon)
                .into_iter()
                .next()
                .map(|candidate| {
                    let label = candidate.label.clone();
                    self.activate_beam_target(&candidate, ctx);
                    label
                });
        self.status = if let Some(radar_label) = radar_label {
            format!("Selected {label} - switched to {radar_label}")
        } else {
            format!("Selected {label}")
        };
        true
    }

    pub(crate) fn select_hazard_index(&mut self, index: usize) {
        self.selected_hazard_index = Some(index);
        self.acknowledge_hazard_index(index);
    }

    pub(crate) fn first_unacknowledged_hazard_index(&self) -> Option<usize> {
        self.hazard_overlay
            .as_ref()?
            .records
            .iter()
            .enumerate()
            .find_map(|(index, record)| {
                (self
                    .unacknowledged_hazard_event_ids
                    .contains(&record.event_id)
                    && hazard_record_should_latch_attention(record))
                .then_some(index)
            })
    }

    pub(crate) fn focus_unacknowledged_hazard_record(
        &mut self,
        index: usize,
        ctx: &egui::Context,
    ) -> bool {
        if let Some(record) = self
            .hazard_overlay
            .as_ref()
            .and_then(|overlay| overlay.records.get(index))
        {
            self.hidden_hazard_families.remove(&record.event_family);
            self.app_settings.current_alert_filter = HazardListFilter::All.key().to_owned();
            self.hazards_visible = true;
        }
        self.focus_hazard_record(index, ctx)
    }

    fn acknowledge_hazard_index(&mut self, index: usize) {
        if let Some(record) = self
            .hazard_overlay
            .as_ref()
            .and_then(|overlay| overlay.records.get(index))
        {
            self.unacknowledged_hazard_event_ids
                .remove(&record.event_id);
        }
    }

    pub(crate) fn acknowledge_all_hazards(&mut self) {
        self.unacknowledged_hazard_event_ids.clear();
    }

    pub(crate) fn acknowledge_all_visible_hazards(&mut self) {
        let Some(overlay) = &self.hazard_overlay else {
            return;
        };
        let visible_ids = overlay
            .records
            .iter()
            .filter(|record| {
                self.hazard_record_visible(record) && hazard_points_renderable(&record.points)
            })
            .map(|record| record.event_id.clone())
            .collect::<Vec<_>>();
        for event_id in visible_ids {
            self.unacknowledged_hazard_event_ids.remove(&event_id);
        }
    }

    pub(crate) fn selected_hazard_record(&self) -> Option<&HazardRecord> {
        let overlay = self.hazard_overlay.as_ref()?;
        let index = self.selected_hazard_index?;
        overlay.records.get(index)
    }

    fn hazard_record_visible(&self, record: &HazardRecord) -> bool {
        self.hazard_record_visible_at_timeline_time(record, self.hazard_overlay_timeline_time())
    }

    fn hazard_overlay_timeline_time(&self) -> Option<DateTime<Utc>> {
        self.event_loop_hazard_window
            .is_some()
            .then(|| self.active_loop_timeline_scan_time_utc())
            .flatten()
    }

    fn hazard_record_visible_at_timeline_time(
        &self,
        record: &HazardRecord,
        frame_time: Option<DateTime<Utc>>,
    ) -> bool {
        if self.hidden_hazard_families.contains(&record.event_family) {
            return false;
        }
        if let Some((start_utc, end_utc)) = self.event_loop_hazard_window {
            let Some(frame_time) = frame_time else {
                return event_loop_hazard_record_intersects_window(record, start_utc, end_utc);
            };
            if frame_time < start_utc || frame_time >= end_utc {
                return false;
            }
            return event_loop_hazard_record_valid_at(record, frame_time);
        }
        if !self.hazards_active_only {
            return true;
        }
        hazard_record_is_active_or_pending(record)
    }

    fn hazard_visibility_signature(&self, frame_time: Option<DateTime<Utc>>) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hazards_active_only.hash(&mut hasher);
        if let Some((start_utc, end_utc)) = self.event_loop_hazard_window {
            start_utc.timestamp_millis().hash(&mut hasher);
            end_utc.timestamp_millis().hash(&mut hasher);
        }
        let Some(overlay) = &self.hazard_overlay else {
            return hasher.finish();
        };
        for (index, record) in overlay.records.iter().enumerate() {
            if self.hazard_record_visible_at_timeline_time(record, frame_time)
                && hazard_points_renderable(&record.points)
            {
                index.hash(&mut hasher);
                record.event_id.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    pub(crate) fn hazard_at_position(
        &self,
        rect: egui::Rect,
        position: egui::Pos2,
    ) -> Option<usize> {
        if !self.hazards_visible {
            return None;
        }
        let overlay = self.hazard_overlay.as_ref()?;
        let (lon, lat) = self.screen_to_lon_lat(rect, position);
        let point = HazardPoint { lon, lat };
        let mut best_containing = None::<(usize, f32, u8)>;
        let mut best_near = None::<(usize, f32, f32, u8)>;
        let mut best_label = None::<(usize, f32, f32, u8)>;
        for (index, record) in overlay.records.iter().enumerate() {
            if !self.hazard_record_visible(record) || !hazard_points_renderable(&record.points) {
                continue;
            }
            let screen_area = self.hazard_screen_area(rect, &record.points);
            let family_order = hazard_family_order(&record.event_family);
            if bbox_contains(record.bbox, point.lon, point.lat)
                && hazard_polygon_contains_point(&record.points, point)
            {
                let candidate = (index, screen_area, family_order);
                if best_containing.is_none_or(|best| {
                    candidate
                        .1
                        .total_cmp(&best.1)
                        .then_with(|| candidate.2.cmp(&best.2))
                        .is_lt()
                }) {
                    best_containing = Some(candidate);
                }
                continue;
            }

            let edge_distance = self.hazard_screen_edge_distance(rect, &record.points, position);
            if edge_distance <= HAZARD_CLICK_TOLERANCE_PX {
                let candidate = (index, edge_distance, screen_area, family_order);
                if best_near.is_none_or(|best| {
                    candidate
                        .1
                        .total_cmp(&best.1)
                        .then_with(|| candidate.2.total_cmp(&best.2))
                        .then_with(|| candidate.3.cmp(&best.3))
                        .is_lt()
                }) {
                    best_near = Some(candidate);
                }
            }

            if self.map_scale >= 62.0 {
                let label_center = self.hazard_screen_centroid(rect, &record.points);
                let label_distance = label_center.distance(position);
                if label_distance <= HAZARD_LABEL_CLICK_RADIUS_PX {
                    let candidate = (index, label_distance, screen_area, family_order);
                    if best_label.is_none_or(|best| {
                        candidate
                            .1
                            .total_cmp(&best.1)
                            .then_with(|| candidate.2.total_cmp(&best.2))
                            .then_with(|| candidate.3.cmp(&best.3))
                            .is_lt()
                    }) {
                        best_label = Some(candidate);
                    }
                }
            }
        }
        best_containing
            .map(|(index, _, _)| index)
            .or_else(|| best_near.map(|(index, _, _, _)| index))
            .or_else(|| best_label.map(|(index, _, _, _)| index))
    }

    fn hazard_screen_area(&self, rect: egui::Rect, points: &[HazardPoint]) -> f32 {
        if points.len() < 3 {
            return 0.0;
        }
        let mut area = 0.0f32;
        let mut previous = self.lon_lat_to_screen(
            rect,
            points[points.len() - 1].lon,
            points[points.len() - 1].lat,
        );
        for point in points {
            let current = self.lon_lat_to_screen(rect, point.lon, point.lat);
            area += previous.x * current.y - current.x * previous.y;
            previous = current;
        }
        area.abs() * 0.5
    }

    pub(crate) fn hazard_screen_centroid(
        &self,
        rect: egui::Rect,
        points: &[HazardPoint],
    ) -> egui::Pos2 {
        let screen_points = points
            .iter()
            .map(|point| self.lon_lat_to_screen(rect, point.lon, point.lat))
            .collect::<Vec<_>>();
        polygon_screen_centroid(&screen_points)
    }

    fn hazard_screen_edge_distance(
        &self,
        rect: egui::Rect,
        points: &[HazardPoint],
        position: egui::Pos2,
    ) -> f32 {
        if points.len() < 2 {
            return f32::INFINITY;
        }
        let mut previous = self.lon_lat_to_screen(
            rect,
            points[points.len() - 1].lon,
            points[points.len() - 1].lat,
        );
        let mut best_distance_sq = f32::INFINITY;
        for point in points {
            let current = self.lon_lat_to_screen(rect, point.lon, point.lat);
            best_distance_sq =
                best_distance_sq.min(point_segment_distance_sq(position, previous, current));
            previous = current;
        }
        best_distance_sq.sqrt()
    }

    pub(crate) fn draw_hazard_fills(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !self.hazards_visible {
            return;
        }
        if self.hazard_overlay.is_none() {
            return;
        }
        let built = self.cached_hazard_overlay_shapes(rect);
        painter.extend(built.fill_shapes.iter().cloned());
    }

    pub(crate) fn draw_hazard_overlays(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !self.hazards_visible {
            return;
        }
        if self.hazard_overlay.is_none() {
            return;
        }
        let built = self.cached_hazard_overlay_shapes(rect);
        painter.extend(built.outline_shapes.iter().cloned());
        self.draw_unacknowledged_hazard_flashes(painter, rect);
        if !self.app_settings.show_hazard_labels {
            return;
        }
        let global = self.style_registry.hazard_global();
        let halo = style_color32(self.style_registry.labels().warning_halo_color);
        let overlay = self.hazard_overlay.as_ref();
        let new_label_color = new_hazard_label_color(painter.ctx());
        for (center, label, selected, record_index) in &built.labels {
            let unacknowledged = overlay
                .and_then(|overlay| overlay.records.get(*record_index))
                .is_some_and(|record| {
                    self.unacknowledged_hazard_event_ids
                        .contains(&record.event_id)
                });
            let display_label = if unacknowledged {
                format!("NEW {label}")
            } else {
                label.clone()
            };
            let text_color = if unacknowledged {
                new_label_color
            } else {
                egui::Color32::from_rgb(245, 248, 250)
            };
            draw_halo_text(
                painter,
                *center,
                egui::Align2::CENTER_CENTER,
                &display_label,
                egui::FontId::proportional(if *selected || unacknowledged {
                    global.label_font_selected_px
                } else {
                    global.label_font_px
                }),
                text_color,
                halo,
            );
        }
    }

    fn draw_unacknowledged_hazard_flashes(&self, painter: &egui::Painter, rect: egui::Rect) {
        if self.event_loop_hazard_window.is_some() {
            return;
        }
        if self.unacknowledged_hazard_event_ids.is_empty() {
            return;
        }
        let Some(overlay) = &self.hazard_overlay else {
            return;
        };
        painter
            .ctx()
            .request_repaint_after(Duration::from_millis(350));
        let blink_on = painter
            .ctx()
            .input(|input| (input.time * 2.4) as i64 % 2 == 0);
        let color = if blink_on {
            egui::Color32::from_rgba_unmultiplied(255, 230, 96, 245)
        } else {
            egui::Color32::from_rgba_unmultiplied(255, 118, 72, 220)
        };
        let stroke = egui::Stroke::new(if blink_on { 4.5 } else { 2.75 }, color);
        let bounds = self.visible_geo_bounds(rect).expand(0.05);
        for record in &overlay.records {
            if !self
                .unacknowledged_hazard_event_ids
                .contains(&record.event_id)
                || !self.hazard_record_visible(record)
                || !hazard_points_renderable(&record.points)
                || !bounds.intersects_bbox(record.bbox)
            {
                continue;
            }
            let points = record
                .points
                .iter()
                .map(|point| self.lon_lat_to_screen(rect, point.lon, point.lat))
                .collect::<Vec<_>>();
            if points.len() < 3 {
                continue;
            }
            let mut shapes = Vec::new();
            let legit_px = hazard_bbox_segment_allowance_px(record.bbox, self.map_scale);
            push_solid_closed_line(&mut shapes, &points, stroke, rect, legit_px);
            painter.extend(shapes);
        }
    }

    pub(crate) fn cached_hazard_overlay_shapes(
        &self,
        rect: egui::Rect,
    ) -> Arc<HazardOverlayShapes> {
        // Polygon projection + ear-clip tessellation is cached per view key:
        // idle repaints reuse it; pan/zoom/selection/content changes rebuild.
        // The generation counter invalidates exactly on overlay replacement.
        use std::hash::{Hash, Hasher};
        let frame_time = self.hazard_overlay_timeline_time();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.view_shape_key(2, rect).hash(&mut hasher);
        self.hazard_overlay_generation.hash(&mut hasher);
        self.selected_hazard_index.hash(&mut hasher);
        // Style edits must repaint: the registry signature covers every
        // styled property (it subsumes the old hazard_fill_alpha hash).
        self.style_registry.signature().hash(&mut hasher);
        self.hazard_visibility_signature(frame_time)
            .hash(&mut hasher);
        for family in &self.hidden_hazard_families {
            family.hash(&mut hasher);
        }
        let key = hasher.finish();
        let mut cache = self.hazard_shape_cache.borrow_mut();
        cache
            .get_or_insert_with(key, || {
                Arc::new(self.build_hazard_overlay_shapes(rect, frame_time))
            })
            .clone()
    }

    pub(crate) fn build_hazard_overlay_shapes(
        &self,
        rect: egui::Rect,
        frame_time: Option<DateTime<Utc>>,
    ) -> HazardOverlayShapes {
        let mut out = HazardOverlayShapes {
            fill_shapes: Vec::new(),
            outline_shapes: Vec::new(),
            labels: Vec::new(),
        };
        let Some(overlay) = &self.hazard_overlay else {
            return out;
        };
        let bounds = self.visible_geo_bounds(rect).expand(0.05);
        let visible_count = overlay
            .records
            .iter()
            .filter(|record| {
                self.hazard_record_visible_at_timeline_time(record, frame_time)
                    && hazard_points_renderable(&record.points)
                    && bounds.intersects_bbox(record.bbox)
            })
            .count();
        let heavy_layer = visible_count > HAZARD_HEAVY_LAYER_FILL_LIMIT && self.map_scale < 240.0;
        let mut label_rects = Vec::<egui::Rect>::new();
        let mut labeled_events = BTreeSet::<String>::new();
        let mut fill_candidates = Vec::<HazardFillCandidate>::new();
        for (index, record) in overlay.records.iter().enumerate() {
            if !self.hazard_record_visible_at_timeline_time(record, frame_time)
                || !hazard_points_renderable(&record.points)
                || !bounds.intersects_bbox(record.bbox)
            {
                continue;
            }
            let points = record
                .points
                .iter()
                .map(|point| self.lon_lat_to_screen(rect, point.lon, point.lat))
                .collect::<Vec<_>>();
            if points.len() < 3 {
                continue;
            }
            let selected = self.selected_hazard_index == Some(index);
            let style = self
                .style_registry
                .hazard_polygon(&record.event_family, hazard_record_style_threat(record));
            let global = self.style_registry.hazard_global();
            let color = style_color32(style.stroke_color);
            let base_alpha = style.fill_alpha.unwrap_or(global.fill_alpha);
            let fill_alpha = hazard_fill_alpha(base_alpha, selected);
            let fill_rgb = style.fill_color;
            let fill = egui::Color32::from_rgba_unmultiplied(
                fill_rgb[0],
                fill_rgb[1],
                fill_rgb[2],
                fill_alpha,
            );
            let stroke_width = style.stroke_width * global.stroke_width_scale
                + if selected {
                    global.selected_width_boost
                } else {
                    0.0
                };
            let stroke = egui::Stroke::new(
                stroke_width,
                egui::Color32::from_rgba_unmultiplied(
                    color.r(),
                    color.g(),
                    color.b(),
                    if selected {
                        global.stroke_alpha_selected
                    } else {
                        global.stroke_alpha
                    },
                ),
            );
            let solid = matches!(style.dash, styles::DashPattern::Solid);
            let legit_px = hazard_bbox_segment_allowance_px(record.bbox, self.map_scale);
            let has_screen_jump = screen_polyline_has_jump(&points, true, rect, legit_px);
            // Perf edge case: a coastline-traced polygon (an Alaska marine SPS,
            // or a big multi-county watch union) can carry thousands of
            // vertices, and the fill runs an O(n^2) self-intersection scan
            // plus a huge per-frame mesh. Above the limit we skip ONLY the
            // fill and let the existing, jump-aware outline path draw the
            // ring verbatim — no geometry is altered, so nothing new can
            // self-intersect. Normal polygons (the overwhelming majority) are
            // well under the limit and unchanged.
            let fill_ok = points.len() <= HAZARD_FILL_VERTEX_LIMIT;
            if (!heavy_layer || selected) && !has_screen_jump && fill_ok {
                // Fills are not pushed directly: same-family same-color fills
                // are flattened after the loop so overlaps paint once
                // (selection boosts alpha, giving the selected record its own
                // color group — it still pops over the flattened layer).
                fill_candidates.push(HazardFillCandidate {
                    family: record.event_family.clone(),
                    fill,
                    points: points.clone(),
                });
            }
            if solid {
                push_solid_closed_line(&mut out.outline_shapes, &points, stroke, rect, legit_px);
            } else {
                match style.dash {
                    styles::DashPattern::Solid => unreachable!("solid handled above"),
                    styles::DashPattern::Dashed { dash, gap } => {
                        push_dashed_closed_line(
                            &mut out.outline_shapes,
                            &points,
                            stroke,
                            dash,
                            gap,
                            rect,
                            legit_px,
                        );
                    }
                    styles::DashPattern::Dotted => {
                        let dot = stroke.width.max(1.0);
                        push_dashed_closed_line(
                            &mut out.outline_shapes,
                            &points,
                            stroke,
                            dot,
                            dot * 2.0,
                            rect,
                            legit_px,
                        );
                    }
                }
            }
            if (!heavy_layer || selected) && self.map_scale >= 62.0 {
                let base_event_id = base_hazard_event_id(&record.event_id);
                if (selected || !labeled_events.contains(base_event_id))
                    && let Some(center) = hazard_visible_label_anchor(&points, rect)
                {
                    let label = hazard_map_label(record);
                    let label_rect = hazard_label_screen_rect(
                        center,
                        &label,
                        selected,
                        global.label_font_px,
                        global.label_font_selected_px,
                    );
                    let collides = !selected
                        && label_rects
                            .iter()
                            .any(|existing| existing.expand(2.0).intersects(label_rect));
                    if !collides {
                        out.labels.push((center, label, selected, index));
                        label_rects.push(label_rect);
                        labeled_events.insert(base_event_id.to_owned());
                    }
                }
            }
        }
        append_flattened_hazard_fill_shapes(fill_candidates, &mut out.fill_shapes);
        out
    }
}

/// `hazard_geom::format_utc_seconds` output ("2026-07-08T22:45:00Z") →
/// the alert row's compact expiry ("22:45Z"). Anything else → None (the
/// full string still lives in the row hover's detail lines).
fn hazard_until_hhmmz(valid_end: &str) -> Option<String> {
    let time = valid_end.get(11..16)?;
    if valid_end.as_bytes().get(10) != Some(&b'T')
        || !valid_end.ends_with('Z')
        || !time
            .chars()
            .all(|character| character.is_ascii_digit() || character == ':')
    {
        return None;
    }
    Some(format!("{time}Z"))
}

/// One alert list row as fixed-order monospace columns:
/// `CODE  ID  OFFICE  until HH:MMZ  NEW` — the office (middle) truncates
/// to the row's character budget; the id, expiry, and NEW flag never do.
/// The id column drops a leading family-code duplicate ("FFW 0062 4" with
/// code "FFW" reads `FFW   0062 4`).
fn hazard_alert_row_text(
    code: &str,
    label: &str,
    office: &str,
    until: Option<&str>,
    unacknowledged: bool,
    max_chars: usize,
) -> String {
    let id = label
        .strip_prefix(code)
        .map(str::trim_start)
        .filter(|rest| !rest.is_empty())
        .unwrap_or(label);
    let head = format!("{code:<5} {id:<9}");
    let mut tail = String::new();
    if let Some(until) = until {
        tail.push_str("  until ");
        tail.push_str(until);
    }
    if unacknowledged {
        tail.push_str("  NEW");
    }
    let office = office.trim();
    let budget = max_chars.saturating_sub(head.chars().count() + tail.chars().count());
    let mut row = head;
    if !office.is_empty() && budget > 2 {
        let space = budget - 2;
        row.push_str("  ");
        if office.chars().count() <= space {
            row.push_str(office);
        } else {
            row.extend(office.chars().take(space.saturating_sub(1)));
            row.push('…');
        }
    }
    row.push_str(&tail);
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn until_hhmmz_reads_format_utc_seconds_only() {
        assert_eq!(
            hazard_until_hhmmz("2026-07-08T22:45:00Z").as_deref(),
            Some("22:45Z")
        );
        assert_eq!(hazard_until_hhmmz(""), None);
        assert_eq!(hazard_until_hhmmz("2026-07-08 22:45:00"), None);
        assert_eq!(hazard_until_hhmmz("not a timestamp at all"), None);
    }

    #[test]
    fn alert_row_text_keeps_fixed_columns_and_truncates_only_the_office() {
        let text = hazard_alert_row_text(
            "TOR",
            "TOR 0031",
            "NWS Des Moines IA",
            Some("22:45Z"),
            false,
            40,
        );
        assert!(text.starts_with("TOR   0031"), "{text}");
        assert!(text.ends_with("until 22:45Z"), "{text}");
        assert!(text.contains("NWS"), "{text}");
        assert!(text.chars().count() <= 40, "{text}");
        // Office truncated with an ellipsis, never the expiry.
        assert!(text.contains('…'), "{text}");

        // Room to spare: the office renders whole.
        let wide = hazard_alert_row_text(
            "TOR",
            "TOR 0031",
            "NWS Des Moines IA",
            Some("22:45Z"),
            false,
            80,
        );
        assert!(wide.contains("NWS Des Moines IA"), "{wide}");

        // The NEW flag survives truncation at the row's tail.
        let flagged = hazard_alert_row_text(
            "SVR",
            "SVR 0653",
            "NWS Storm Prediction Center",
            Some("03:00Z"),
            true,
            40,
        );
        assert!(flagged.ends_with("NEW"), "{flagged}");
        assert!(flagged.contains("until 03:00Z"), "{flagged}");
        assert!(flagged.chars().count() <= 40, "{flagged}");
    }

    #[test]
    fn alert_row_text_strips_the_family_code_prefix_and_survives_misses() {
        // Multi-zone label: the id column keeps the zone suffix.
        let text = hazard_alert_row_text("FFW", "FFW 0062 4", "KLMK", Some("22:45Z"), false, 60);
        assert!(text.starts_with("FFW   0062 4"), "{text}");
        assert!(!text.contains("FFW 0062"), "{text}");
        // Code that is not a label prefix (watches, MDs): label kept whole.
        let watch = hazard_alert_row_text("Watch", "TOA 0123", "SPC", None, false, 60);
        assert!(watch.contains("TOA 0123"), "{watch}");
        // No office, no expiry: just the head columns.
        let bare = hazard_alert_row_text("MD", "MD 1234", "", None, false, 60);
        assert!(bare.trim_end().ends_with("1234"), "{bare}");
    }
}
