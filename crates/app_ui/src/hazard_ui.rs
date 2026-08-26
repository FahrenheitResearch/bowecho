//! Hazard panel UI and hazard map-paint methods moved verbatim out of
//! `main.rs` (v0.29.4 decomposition, queue item #3). Hazard types, statics,
//! and constants stay in `main.rs` (pure geometry in `hazard_geom.rs`);
//! this module reaches them via `crate::`.

use crate::*;

/// Total cached projected/tessellated hazard geometry, across panes and view
/// keys. 64 MiB leaves room for millions of path points or several million
/// mesh elements, while preventing eight dense warning-day entries from each
/// retaining an independently large allocation.
const HAZARD_SHAPE_CACHE_GEOMETRY_BUDGET: usize = 64 * 1024 * 1024;

/// Estimated geometry bytes for one egui shape, including recursively nested
/// `Shape::Vec` children and the variable-sized geometry payloads. Fixed-size
/// variants are fully represented by `size_of::<Shape>()`.
fn egui_shape_geometry_weight(shape: &egui::Shape) -> usize {
    let payload = match shape {
        egui::Shape::Vec(shapes) => shapes.iter().fold(0usize, |weight, shape| {
            weight.saturating_add(egui_shape_geometry_weight(shape))
        }),
        egui::Shape::Path(path) => std::mem::size_of_val(path.points.as_slice()),
        egui::Shape::Mesh(mesh) => mesh.bytes_used(),
        egui::Shape::Text(text) => {
            // Hazard labels are stored separately, but keep the estimator
            // sound if a future overlay caches tessellated text shapes.
            std::mem::size_of_val(text.galley.as_ref())
                .saturating_add(
                    text.galley
                        .num_vertices
                        .saturating_mul(std::mem::size_of::<egui::epaint::Vertex>()),
                )
                .saturating_add(
                    text.galley
                        .num_indices
                        .saturating_mul(std::mem::size_of::<u32>()),
                )
                .saturating_add(text.galley.job.text.len())
        }
        egui::Shape::Noop
        | egui::Shape::Circle(_)
        | egui::Shape::Ellipse(_)
        | egui::Shape::LineSegment { .. }
        | egui::Shape::Rect(_)
        | egui::Shape::QuadraticBezier(_)
        | egui::Shape::CubicBezier(_)
        | egui::Shape::Callback(_) => 0,
    };
    std::mem::size_of::<egui::Shape>().saturating_add(payload)
}

fn hazard_overlay_geometry_weight(overlay: &HazardOverlayShapes) -> usize {
    let shape_weight = overlay
        .fill_shapes
        .iter()
        .chain(&overlay.outline_shapes)
        .fold(0usize, |weight, shape| {
            weight.saturating_add(egui_shape_geometry_weight(shape))
        });
    let label_weight = overlay.labels.iter().fold(
        std::mem::size_of_val(overlay.labels.as_slice()),
        |weight, (_, text, _, _)| weight.saturating_add(text.len()),
    );
    std::mem::size_of::<HazardOverlayShapes>()
        .saturating_add(shape_weight)
        .saturating_add(label_weight)
}

impl ViewerApp {
    pub(crate) fn hazard_panel(&mut self, ui: &mut egui::Ui) {
        let rendered_section = self.sidebar_section_render.map(|render| render.section);
        let show_controls = rendered_section.is_none()
            || rendered_section == Some(sidebar_layout::SectionId::AlertsControls);
        let mut europe_list_only = matches!(
            self.resolved_warning_source(),
            meteoalarm::ResolvedWarningSource::MeteoAlarm(_)
        );
        if show_controls {
            let mut source_changed = false;
            panel_kit::row(ui, "Source", |ui| {
                let mut mode =
                    meteoalarm::WarningSourceMode::from_key(&self.app_settings.warning_source);
                egui::ComboBox::from_id_salt("live_warning_source")
                    .selected_text(mode.label())
                    .width(174.0)
                    .show_ui(ui, |ui| {
                        for option in meteoalarm::WarningSourceMode::ALL {
                            ui.selectable_value(&mut mode, option, option.label());
                        }
                    });
                if mode.key() != self.app_settings.warning_source {
                    self.app_settings.warning_source = mode.key().to_owned();
                    source_changed = true;
                }
            });

            let mode = meteoalarm::WarningSourceMode::from_key(&self.app_settings.warning_source);
            let radar_country = data_source::sites::resolve(&self.display_owner_site())
                .and_then(|site| meteoalarm::country_feed_for_label(&site.country));
            if mode == meteoalarm::WarningSourceMode::Europe {
                panel_kit::row(ui, "Country", |ui| {
                    let selected = meteoalarm::country_feed_by_slug(
                        self.app_settings.meteoalarm_country.trim(),
                    );
                    let selected_label = selected
                        .map(|country| country.label)
                        .unwrap_or("Auto (radar country)");
                    egui::ComboBox::from_id_salt("meteoalarm_country")
                        .selected_text(selected_label)
                        .width(174.0)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(selected.is_none(), "Auto (radar country)")
                                .clicked()
                            {
                                self.app_settings.meteoalarm_country = "auto".to_owned();
                                source_changed = true;
                            }
                            for country in meteoalarm::COUNTRY_FEEDS {
                                if ui
                                    .selectable_label(selected == Some(*country), country.label)
                                    .clicked()
                                {
                                    self.app_settings.meteoalarm_country = country.slug.to_owned();
                                    source_changed = true;
                                }
                            }
                        });
                });
            } else if mode == meteoalarm::WarningSourceMode::Auto
                && let Some(country) = radar_country
            {
                ui.weak(format!("Auto source: MeteoAlarm {}", country.label));
            }
            if source_changed {
                self.mark_app_settings_dirty();
                self.reload_warning_source(ui.ctx());
                return;
            }

            let resolved_source = self.resolved_warning_source();
            if let meteoalarm::ResolvedWarningSource::Unavailable(reason) = &resolved_source {
                panel_kit::status_block(
                    ui,
                    reason,
                    Some("Change Source above, or select a radar in the matching warning network."),
                );
                return;
            }
            europe_list_only = matches!(
                resolved_source,
                meteoalarm::ResolvedWarningSource::MeteoAlarm(_)
            );
            if europe_list_only {
                panel_kit::status_block(
                    ui,
                    "Official MeteoAlarm country warnings · list and details",
                    Some(
                        "MeteoAlarm's anonymous Atom/CAP feed carries area names and region codes but no public polygon geometry. BowEcho does not invent placement, so these entries are intentionally not drawn on the map.",
                    ),
                );
                ui.weak("Data provided by EUMETNET members via MeteoAlarm · CC BY 4.0.");
                ui.horizontal_wrapped(|ui| {
                    ui.hyperlink_to("Official MeteoAlarm feeds", "https://feeds.meteoalarm.org/");
                    ui.weak("·");
                    ui.hyperlink_to(
                        "CC BY 4.0 license",
                        "https://creativecommons.org/licenses/by/4.0/",
                    );
                });
            }

            // Wrapped rather than a kit row: these live controls must fit the
            // narrow sidebar. Map geometry controls are NWS-only; active/refresh
            // semantics apply to both sources.
            let mut startup_defaults_changed = false;
            ui.horizontal_wrapped(|ui| {
                if !europe_list_only {
                    if ui
                        .checkbox(&mut self.hazards_visible, "Show on map")
                        .on_hover_text("Draw NWS warning polygons on the map")
                        .changed()
                    {
                        startup_defaults_changed = true;
                    }
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
                }
                if ui
                    .checkbox(&mut self.hazards_active_only, "Active only")
                    .on_hover_text("Hide expired/cancelled alerts")
                    .changed()
                {
                    startup_defaults_changed = true;
                }
                if ui
                    .checkbox(&mut self.live_hazard_auto_refresh, "Auto-refresh")
                    .on_hover_text("Re-fetch active alerts on the live cadence")
                    .changed()
                {
                    startup_defaults_changed = true;
                }
            });
            if startup_defaults_changed {
                self.persist_hazard_panel_settings();
            }
            if !europe_list_only {
                // Family filters as a kit chip grid: selected = shown on the map
                // and in the list; the hidden-family set is the same state the
                // checkboxes used to edit.
                let family_chips = HAZARD_FILTER_FAMILIES
                    .iter()
                    .map(|&(family, label)| panel_kit::Chip {
                        label,
                        hotkey: None,
                        selected: !self.hidden_hazard_families.contains(family),
                        enabled: true,
                        hover: Some(format!("Show {family} alerts on the map and in the list")),
                    })
                    .collect::<Vec<_>>();
                if let Some(clicked) = panel_kit::chip_grid(ui, &family_chips) {
                    let (family, _) = HAZARD_FILTER_FAMILIES[clicked];
                    if !self.hidden_hazard_families.remove(family) {
                        self.hidden_hazard_families.insert(family.to_owned());
                    }
                    self.persist_hazard_panel_settings();
                    if self
                        .selected_hazard_record()
                        .is_some_and(|record| !self.hazard_record_visible(record))
                    {
                        self.selected_hazard_index = None;
                    }
                    ui.ctx().request_repaint();
                }
                ui.weak("Watch types");
                let watch_parent_visible = !self.hidden_hazard_families.contains("watch");
                let watch_chips = HAZARD_WATCH_FILTERS
                    .iter()
                    .map(|&(watch_type, label)| panel_kit::Chip {
                        label,
                        hotkey: None,
                        selected: (watch_parent_visible || watch_type == "pds")
                            && !self
                                .app_settings
                                .hidden_hazard_watch_types
                                .iter()
                                .any(|hidden| hidden.eq_ignore_ascii_case(watch_type)),
                        enabled: true,
                        hover: Some(if watch_type == "pds" {
                            "Show PDS watches even when the general Watch family is hidden"
                                .to_owned()
                        } else {
                            format!("Show {label} polygons")
                        }),
                    })
                    .collect::<Vec<_>>();
                if let Some(clicked) = panel_kit::chip_grid(ui, &watch_chips) {
                    let watch_type = HAZARD_WATCH_FILTERS[clicked].0;
                    if !watch_parent_visible && watch_type != "pds" {
                        self.hidden_hazard_families.remove("watch");
                        self.app_settings
                            .hidden_hazard_watch_types
                            .retain(|hidden| !hidden.eq_ignore_ascii_case(watch_type));
                        self.persist_hazard_panel_settings();
                    } else if let Some(index) = self
                        .app_settings
                        .hidden_hazard_watch_types
                        .iter()
                        .position(|hidden| hidden.eq_ignore_ascii_case(watch_type))
                    {
                        self.app_settings.hidden_hazard_watch_types.remove(index);
                        self.mark_app_settings_dirty();
                    } else {
                        self.app_settings
                            .hidden_hazard_watch_types
                            .push(watch_type.to_owned());
                        self.mark_app_settings_dirty();
                    }
                    ui.ctx().request_repaint();
                }
                // The ordinary fill slider is authoritative for every family;
                // per-family alpha remains available in Appearance for advanced
                // customization after this global control is used.
                let mut fill_alpha = self.style_registry.hazard_global().fill_alpha as f32;
                let fill_response = panel_kit::slider_row(
                    ui,
                    "All fills",
                    &mut fill_alpha,
                    0.0..=80.0,
                    0.0,
                    |value| format!("{value:.0}"),
                )
                .on_hover_text(
                    "Set warning-polygon fill opacity for every family (0 disables fills)",
                );
                if fill_response.changed() {
                    self.set_all_hazard_fill_alpha(fill_alpha.round() as u8);
                    ui.ctx().request_repaint();
                }
                if fill_response.drag_stopped()
                    || (fill_response.changed() && !fill_response.dragged())
                {
                    self.save_styles();
                }
            }
            ui.horizontal(|ui| {
                let loading = self.hazard_receiver.is_some();
                if fixed_action_button(ui, "Refresh Live", 96.0).clicked() && !loading {
                    self.refresh_live_hazards_manually(ui.ctx());
                }
                if fixed_action_button(ui, "Clear", 52.0).clicked() {
                    self.hazard_overlay_generation = self.hazard_overlay_generation.wrapping_add(1);
                    self.invalidate_hazard_record_metadata_cache();
                    self.hazard_overlay = None;
                    self.completed_live_hazard_overlay = None;
                    self.selected_hazard_index = None;
                    self.unacknowledged_hazard_event_ids.clear();
                    self.hazard_status = if europe_list_only {
                        "No MeteoAlarm warning entries loaded".to_owned()
                    } else {
                        "No hazard polygons loaded".to_owned()
                    };
                }
            });
        }

        self.remembered_section(
            ui,
            "severe_current_alerts",
            "Current alerts",
            true,
            |app, ui| {
                let rows = app.visible_hazard_list_rows();
                let record_metadata = app.cached_hazard_record_metadata();
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
                    ui.weak(if europe_list_only {
                        "No active warnings returned for this country"
                    } else {
                        "No hazard polygons loaded"
                    });
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
                    ui.ctx().request_repaint_after(Duration::from_millis(350));
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
                        // Monospace advance measured the same way select_row
                        // lays its text out, so the character budget matches
                        // what actually fits the row.
                        let char_width = ui
                            .painter()
                            .layout_no_wrap(
                                "0".to_owned(),
                                egui::FontId::monospace(12.0),
                                egui::Color32::WHITE,
                            )
                            .size()
                            .x
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
                            let family_accent = app
                                .hazard_overlay
                                .as_ref()
                                .and_then(|overlay| overlay.records.get(row.index))
                                .map(|record| {
                                    meteoalarm::record_accent_color(record).unwrap_or_else(|| {
                                        style_color32(
                                            app.style_registry
                                                .hazard_polygon(
                                                    &record.event_family,
                                                    record_metadata
                                                        .get(row.index)
                                                        .map(|metadata| {
                                                            hazard_record_style_threat_with_pds(
                                                                record,
                                                                metadata.pds_watch,
                                                            )
                                                        })
                                                        .unwrap_or_else(|| {
                                                            hazard_record_style_threat(record)
                                                        }),
                                                )
                                                .stroke_color,
                                        )
                                    })
                                });
                            let accent = current_alert_accent_color(
                                family_accent,
                                row.unacknowledged,
                                ui.input(|input| input.time),
                            );
                            let response = panel_kit::select_row(
                                ui,
                                row.selected,
                                true,
                                &text,
                                Some(row.hover.as_str()),
                            );
                            if let Some(accent) = accent {
                                let accent_rect = egui::Rect::from_min_max(
                                    egui::pos2(
                                        response.rect.left() + 1.0,
                                        response.rect.top() + 1.0,
                                    ),
                                    egui::pos2(
                                        response.rect.left() + 4.0,
                                        response.rect.bottom() - 1.0,
                                    ),
                                );
                                ui.painter().rect_filled(accent_rect, 1.0, accent);
                            }
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

        if rendered_section.is_none()
            || rendered_section == Some(sidebar_layout::SectionId::AlertsCurrent)
        {
            if let Some(record) = self.selected_hazard_record() {
                ui.add_space(6.0);
                egui::CollapsingHeader::new("Selected alert text")
                    .id_salt("hazard_selected_alert_text")
                    .default_open(true)
                    .show(ui, |ui| {
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
                        spc_md_image::show(
                            ui,
                            &record.event_family,
                            record.source_url.as_deref(),
                            &record.event_id,
                        );
                        if meteoalarm::is_meteoalarm_record(record)
                            && let Some(source_url) = record.source_url.as_deref()
                        {
                            ui.hyperlink_to("Open official CAP warning", source_url);
                        }
                    });
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
        }

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
            panel_kit::row(ui, "Wide view", |ui| {
                let mut outline_only = app
                    .app_settings
                    .overlay_spc_outline_only_wide_zoom;
                if ui
                    .checkbox(&mut outline_only, "Outlines only when zoomed out")
                    .on_hover_text(
                        "At wide map scales, keep SPC/ESTOFEX outlines and labels but skip polygon fills. Zooming in restores shading.",
                    )
                    .changed()
                {
                    app.app_settings.overlay_spc_outline_only_wide_zoom = outline_only;
                    app.mark_app_settings_dirty();
                    ui.ctx().request_repaint();
                }
            });
            if app.spc_day == 1 {
                let outlook_date = app.spc_outlook_date();
                let now = Utc::now();
                panel_kit::row(ui, "Issuance", |ui| {
                    let selected_future = app
                        .spc_day1_issue
                        .is_not_yet_issued(outlook_date, now);
                    let selected_text = if selected_future {
                        format!("{} - not yet issued", app.spc_day1_issue.label())
                    } else {
                        app.spc_day1_issue.label().to_owned()
                    };
                    egui::ComboBox::from_id_salt("severe_spc_day1_issue")
                        .selected_text(selected_text)
                        .width(ui.available_width().clamp(120.0, 220.0))
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_value(
                                    &mut app.spc_day1_issue,
                                    spc_layers::SpcDay1Issue::Auto,
                                    "Auto / latest",
                                )
                                .changed()
                            {
                                changed = true;
                            }
                            for issue in spc_layers::SPC_DAY1_FIXED_ISSUES {
                                let not_yet_issued =
                                    issue.is_not_yet_issued(outlook_date, now);
                                let label = if not_yet_issued {
                                    format!("{} - not yet issued", issue.label())
                                } else {
                                    issue.label().to_owned()
                                };
                                let response = ui
                                    .add_enabled_ui(!not_yet_issued, |ui| {
                                        ui.selectable_value(
                                            &mut app.spc_day1_issue,
                                            issue,
                                            label,
                                        )
                                    })
                                    .inner;
                                if response.changed() {
                                    changed = true;
                                }
                            }
                        })
                        .response
                        .on_hover_text(
                            "Auto preserves the current/live behavior. A fixed issue loads one official SPC Day-1 archive slot for the displayed date; SPC's 06Z issue is stored in its 12Z-valid archive file.",
                        );
                });

                let issue_status = if app.spc_rx.is_some() {
                    format!(
                        "SPC {} {}: fetching official GeoJSON",
                        outlook_date,
                        app.spc_day1_issue.label()
                    )
                } else {
                    match app.spc_data.day1_issue_status.as_ref() {
                        Some(spc_layers::SpcDay1IssueStatus::AutoArchiveLoaded {
                            date,
                            issue,
                            loaded,
                            requested,
                        }) => {
                            let coverage = if loaded == requested {
                                String::new()
                            } else {
                                format!(" ({loaded}/{requested} selected products available)")
                            };
                            format!(
                                "Auto: {date} {} was the latest available archive issue{coverage}",
                                issue.label()
                            )
                        }
                        Some(spc_layers::SpcDay1IssueStatus::AutoArchiveMissing { date }) => {
                            format!("Auto: no official Day-1 archive issue was found for {date}")
                        }
                        Some(spc_layers::SpcDay1IssueStatus::SelectedLoaded {
                            date,
                            issue,
                            loaded,
                            requested,
                        }) => {
                            let coverage = if loaded == requested {
                                String::new()
                            } else {
                                format!(" - {loaded}/{requested} selected products available")
                            };
                            format!(
                                "Showing official {date} {} issuance{coverage}",
                                issue.label()
                            )
                        }
                        Some(spc_layers::SpcDay1IssueStatus::SelectedNotYetIssued {
                            date,
                            issue,
                        }) => format!("{date} {} has not been issued yet", issue.label()),
                        Some(spc_layers::SpcDay1IssueStatus::SelectedMissing {
                            date,
                            issue,
                        }) => format!(
                            "Official {date} {} archive file is missing or unavailable",
                            issue.label()
                        ),
                        Some(spc_layers::SpcDay1IssueStatus::NoStandardProductSelected) => {
                            "Select Categorical, Tornado, Wind, or Hail to load this SPC issuance"
                                .to_owned()
                        }
                        None if app.spc_day1_issue == spc_layers::SpcDay1Issue::Auto => {
                            "Auto uses SPC's current headline outlook for today's live view"
                                .to_owned()
                        }
                        None => "Waiting to fetch the selected official issuance".to_owned(),
                    }
                };
                ui.weak(issue_status);
            }
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
                        enabled: true,
                        hover: Some("Outlook kind — drawn in SPC's own colors".to_owned()),
                    }
                })
                .collect::<Vec<_>>();
            let reports_chip = outlook_chips.len();
            outlook_chips.push(panel_kit::Chip {
                label: "Reports",
                hotkey: None,
                selected: app.spc_reports_enabled,
                enabled: true,
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
                enabled: true,
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
                app.invalidate_spc_fetch_request();
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
                            "How often BowEcho re-fetches the selected built-in warning source. \
                             NWS guidance is 30 s; MeteoAlarm and custom/relay feeds use the same \
                             operator-controlled cadence.",
                        )
                        .changed()
                    {
                        app.app_settings.warning_refresh_seconds =
                            secs.max(MIN_LIVE_HAZARD_REFRESH_SECONDS);
                        app.mark_app_settings_dirty();
                        ui.ctx().request_repaint();
                    }
                });
                if matches!(
                    app.resolved_warning_source(),
                    meteoalarm::ResolvedWarningSource::Nws
                ) {
                    ui.label("Custom provider (poll URL)").on_hover_text(
                        "Optional http(s) URL BowEcho polls alongside NWS active alerts and merges \
                         into the warnings layer. Accepts the NWS CAP/GeoJSON alert FeatureCollection \
                         or NWS text/VTEC + lat/lon polygon format.",
                    );
                    // The URL input truncates its content instead of forcing
                    // the panel wider (320 pt rule).
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut app.app_settings.warning_provider_url)
                            .desired_width(ui.available_width())
                            .hint_text("https://host/warnings.geojson"),
                    );
                    if response.lost_focus() {
                        app.mark_app_settings_dirty();
                    }
                    if !app.app_settings.warning_provider_url.trim().is_empty()
                        && custom_warning_provider_url(&app.app_settings.warning_provider_url)
                            .is_none()
                    {
                        ui.colored_label(
                            egui::Color32::from_rgb(255, 170, 80),
                            "Provider URL must start with http:// or https://",
                        );
                    }
                } else {
                    ui.weak(
                        "Custom GeoJSON providers are only merged with the NWS source; MeteoAlarm uses its official country feed directly.",
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

    fn persist_hazard_panel_settings(&mut self) {
        self.app_settings.hazards_visible = self.hazards_visible;
        self.app_settings.hazards_active_only = self.hazards_active_only;
        self.app_settings.live_hazard_auto_refresh = self.live_hazard_auto_refresh;
        self.app_settings.hidden_hazard_families =
            self.hidden_hazard_families.iter().cloned().collect();
        self.mark_app_settings_dirty();
    }

    fn hazard_summary_lines(&self) -> Vec<String> {
        let mut lines = vec![self.hazard_status.clone()];
        if let Some(overlay) = &self.hazard_overlay {
            if meteoalarm::is_meteoalarm_overlay(overlay) {
                lines.push(format!(
                    "{} scanned, {} active warning entries · list only",
                    overlay.scanned_items, overlay.parsed_items
                ));
            } else {
                lines.push(format!(
                    "{} scanned, {} parsed, {} polygons",
                    overlay.scanned_items, overlay.parsed_items, overlay.polygon_records
                ));
            }
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
        let metadata = self.cached_hazard_record_metadata();
        let frame_time = self.hazard_overlay_timeline_time();
        let filter = HazardListFilter::from_key(&self.app_settings.current_alert_filter);
        let mut rows = overlay
            .records
            .iter()
            .enumerate()
            .filter(|(index, record)| {
                let record_metadata = metadata[*index];
                self.hazard_record_visible_at_timeline_time_with_metadata(
                    record,
                    frame_time,
                    record_metadata,
                ) && (record_metadata.renderable || meteoalarm::is_meteoalarm_record(record))
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
        let metadata = self.cached_hazard_record_metadata();
        let mut rows = overlay
            .records
            .iter()
            .enumerate()
            .filter(|(index, record)| {
                self.unacknowledged_hazard_event_ids
                    .contains(&record.event_id)
                    && metadata[*index].renderable
                    && hazard_record_is_active_or_pending(record)
                    && record.event_family != "local storm report"
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
        let metadata = self.cached_hazard_record_metadata();
        let record_metadata = metadata.get(index).copied();
        let frame_time = self.hazard_overlay_timeline_time();
        if let Some(record) = self
            .hazard_overlay
            .as_ref()
            .and_then(|overlay| overlay.records.get(index))
            && record_metadata.is_some_and(|metadata| {
                self.hazard_record_visible_at_timeline_time_with_metadata(
                    record, frame_time, metadata,
                )
            })
            && meteoalarm::is_meteoalarm_record(record)
        {
            let label = record.label.clone();
            self.select_hazard_index(index);
            self.status = format!("Selected {label} · MeteoAlarm list-only warning");
            ctx.request_repaint();
            return true;
        }
        let Some((bbox, label)) = self
            .hazard_overlay
            .as_ref()
            .and_then(|overlay| overlay.records.get(index))
            .and_then(|record| {
                record_metadata
                    .filter(|metadata| {
                        metadata.renderable
                            && self.hazard_record_visible_at_timeline_time_with_metadata(
                                record, frame_time, *metadata,
                            )
                    })
                    .map(|_| (record.bbox, record.label.clone()))
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

    /// Map-polygon selection keeps ordinary warning clicks lightweight, but
    /// an SPC mesoscale discussion has substantial text and an official
    /// graphic in the Alerts detail view. Route that click directly there
    /// and reveal a collapsed sidebar so the selection is immediately useful.
    pub(crate) fn select_hazard_from_map(&mut self, index: usize) {
        let opens_alert_details = self
            .hazard_overlay
            .as_ref()
            .and_then(|overlay| overlay.records.get(index))
            .is_some_and(|record| record.event_family == "mesoscale discussion");
        self.select_hazard_index(index);
        if opens_alert_details {
            self.reveal_sidebar_section(sidebar_layout::SectionId::AlertsCurrent);
            self.sidebar_hidden = false;
        }
    }

    /// Open the compact warning stack for a map click. Mesoscale discussions
    /// preserve their established direct-to-Alerts behavior; the card stack is
    /// for warning/watch polygons and can contain every exact overlap.
    pub(crate) fn open_hazard_popup_from_map(
        &mut self,
        rect: egui::Rect,
        position: egui::Pos2,
    ) -> bool {
        let hits = self.hazards_at_position(rect, position);
        let Some(&first_hit) = hits.first() else {
            self.hazard_map_popup = None;
            return false;
        };
        let warning_hits = self
            .hazard_overlay
            .as_ref()
            .map(|overlay| {
                hits.into_iter()
                    .filter(|&index| {
                        overlay
                            .records
                            .get(index)
                            .is_some_and(|record| record.event_family != "mesoscale discussion")
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if warning_hits.is_empty() {
            self.hazard_map_popup = None;
            self.select_hazard_from_map(first_hit);
            return true;
        }

        let event_ids = self
            .hazard_overlay
            .as_ref()
            .map(|overlay| {
                warning_hits
                    .iter()
                    .filter_map(|&index| overlay.records.get(index))
                    .map(|record| base_hazard_event_id(&record.event_id).to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let (lon, lat) = self.screen_to_lon_lat(rect, position);
        self.select_hazard_from_map(warning_hits[0]);
        self.hazard_map_popup = Some(HazardMapPopup {
            anchor: HazardPoint { lon, lat },
            event_ids,
            pane_index: if self.grid_layout == PanelLayout::One {
                0
            } else {
                self.active_pane
            },
            screen_rect: None,
            #[cfg(test)]
            layout_probe: None,
        });
        true
    }

    pub(crate) fn hazard_popup_owns_pointer(&self, position: egui::Pos2) -> bool {
        self.hazard_map_popup
            .as_ref()
            .and_then(|popup| popup.screen_rect)
            .is_some_and(|rect| rect.expand(2.0).contains(position))
    }

    /// Foreground, geo-anchored warning cards. The map rectangle is the pane
    /// that owns the clicked polygon, so the pointer tracks pan/zoom and the
    /// whole stack remains clamped to that view.
    pub(crate) fn show_hazard_map_popup(&mut self, ctx: &egui::Context, map_rect: egui::Rect) {
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.hazard_map_popup = None;
            return;
        }
        let Some(popup) = self.hazard_map_popup.as_ref() else {
            return;
        };
        let anchor_geo = popup.anchor;
        let wanted_ids = popup.event_ids.clone();
        let mut cards = Vec::<(usize, String, HazardRecord, styles::PolygonStyle)>::new();
        let metadata = self.cached_hazard_record_metadata();
        let frame_time = self.hazard_overlay_timeline_time();
        if let Some(overlay) = &self.hazard_overlay {
            for wanted_id in &wanted_ids {
                if let Some((index, record)) =
                    overlay.records.iter().enumerate().find(|(index, record)| {
                        let record_metadata = metadata[*index];
                        base_hazard_event_id(&record.event_id) == wanted_id
                            && record.event_family != "mesoscale discussion"
                            && self.hazard_record_visible_at_timeline_time_with_metadata(
                                record,
                                frame_time,
                                record_metadata,
                            )
                            && record_metadata.renderable
                    })
                {
                    let record_metadata = metadata[index];
                    let style = self
                        .style_registry
                        .hazard_polygon(
                            &record.event_family,
                            hazard_record_style_threat_with_pds(record, record_metadata.pds_watch),
                        )
                        .clone();
                    cards.push((index, wanted_id.clone(), record.clone(), style));
                }
            }
        }
        if cards.is_empty() {
            self.hazard_map_popup = None;
            return;
        }
        // Drop expired/removed/hidden records from the live stack immediately.
        if let Some(popup) = self.hazard_map_popup.as_mut() {
            popup.event_ids = cards.iter().map(|(_, id, _, _)| id.clone()).collect();
        }

        let anchor = self.lon_lat_to_screen(map_rect, anchor_geo.lon, anchor_geo.lat);
        if !map_rect.expand(28.0).contains(anchor) {
            self.hazard_map_popup = None;
            return;
        }
        let layout = hazard_popup_layout(map_rect, anchor, cards.len());
        let global = self.style_registry.hazard_global().clone();
        let archive_timeline = self.event_loop_hazard_window.is_some();
        let time_zone = self.time_zone();
        let heading = if archive_timeline {
            self.active_loop_timeline_scan_time_utc().map_or_else(
                || "Warnings at selected time".to_owned(),
                |time| format!("Warnings at {}", time_zone.format_hm(time)),
            )
        } else {
            "Current warnings".to_owned()
        };
        let now = Utc::now();
        let mut close_all = false;
        let mut details_index = None::<usize>;
        #[cfg(test)]
        let mut layout_probe = HazardMapPopupLayoutProbe::default();

        let area = egui::Area::new(egui::Id::new("hazard_map_popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(layout.position)
            .show(ctx, |ui| {
                ui.set_width(layout.width);
                ui.set_min_width(layout.width);
                ui.set_max_width(layout.width);
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(7))
                    .show(ui, |ui| {
                        let inner_width = ui.available_width().max(120.0);
                        ui.set_min_width(inner_width);
                        ui.set_max_width(inner_width);
                        ui.horizontal(|ui| {
                            let heading_width =
                                (ui.available_width() - 26.0 - ui.spacing().item_spacing.x)
                                    .max(72.0);
                            ui.add_sized(
                                [heading_width, ui.spacing().interact_size.y],
                                egui::Label::new(egui::RichText::new(&heading).strong()),
                            );
                            if ui
                                .small_button("X")
                                .on_hover_text("Dismiss warning cards")
                                .clicked()
                            {
                                close_all = true;
                            }
                        });
                        ui.add_space(3.0);
                        let _scroll = egui::ScrollArea::vertical()
                            .id_salt("hazard_map_popup_scroll")
                            .auto_shrink([false, true])
                            .max_height(layout.body_height)
                            .show(ui, |ui| {
                                let scroll_width = ui.available_width().max(112.0);
                                ui.set_min_width(scroll_width);
                                ui.set_max_width(scroll_width);
                                for (card_number, (index, _event_id, record, style)) in
                                    cards.iter().enumerate()
                                {
                                    if card_number > 0 {
                                        ui.add_space(5.0);
                                    }
                                    let fill = hazard_popup_card_fill(
                                        ui.visuals().window_fill(),
                                        style.fill_color,
                                        style.fill_alpha.unwrap_or(global.fill_alpha),
                                    );
                                    let card = egui::Frame::new()
                                        .fill(fill)
                                        .corner_radius(egui::CornerRadius::same(
                                            HAZARD_POPUP_CARD_RADIUS,
                                        ))
                                        .inner_margin(egui::Margin::symmetric(9, 7))
                                        .show(ui, |ui| {
                                            let card_width = ui.available_width().max(96.0);
                                            ui.set_width(card_width);
                                            ui.set_min_width(card_width);
                                            ui.set_max_width(card_width);
                                            let title_width = ui.available_width().max(64.0);
                                            let _title = ui
                                                .vertical(|ui| {
                                                    ui.set_width(title_width);
                                                    ui.set_min_width(title_width);
                                                    ui.set_max_width(title_width);
                                                    ui.add(
                                                        egui::Label::new(
                                                            egui::RichText::new(
                                                                hazard_popup_title(record),
                                                            )
                                                            .strong()
                                                            .size(15.0),
                                                        )
                                                        .wrap(),
                                                    )
                                                })
                                                .inner;
                                            #[cfg(test)]
                                            if card_number == 0 {
                                                layout_probe.title = Some(_title.rect);
                                            }
                                            let metrics = hazard_popup_metric_lines(record);
                                            for metric in metrics {
                                                let _response = ui.label(&metric);
                                                #[cfg(test)]
                                                if card_number == 0 {
                                                    if metric.starts_with("Wind:") {
                                                        layout_probe.wind = Some(_response.rect);
                                                    } else if metric.starts_with("Hail:") {
                                                        layout_probe.hail = Some(_response.rect);
                                                    }
                                                }
                                            }
                                            let mut footer_text = hazard_popup_expiry_text(
                                                record,
                                                now,
                                                archive_timeline,
                                                time_zone,
                                            );
                                            let office = hazard_popup_office(record);
                                            if !office.is_empty() {
                                                footer_text
                                                    .push_str(&format!(" \u{00b7} WFO {office}"));
                                            }
                                            ui.horizontal(|ui| {
                                                let details_width = 58.0;
                                                let footer_width = (ui.available_width()
                                                    - details_width
                                                    - ui.spacing().item_spacing.x)
                                                    .max(54.0);
                                                let _footer = ui
                                                    .vertical(|ui| {
                                                        ui.set_width(footer_width);
                                                        ui.set_min_width(footer_width);
                                                        ui.set_max_width(footer_width);
                                                        ui.add(
                                                            egui::Label::new(
                                                                egui::RichText::new(&footer_text)
                                                                    .weak(),
                                                            )
                                                            .wrap(),
                                                        )
                                                    })
                                                    .inner;
                                                let details = ui.small_button("Details");
                                                #[cfg(test)]
                                                if card_number == 0 {
                                                    layout_probe.footer = Some(_footer.rect);
                                                    layout_probe.details = Some(details.rect);
                                                }
                                                if details.clicked() {
                                                    details_index = Some(*index);
                                                }
                                            });
                                        });
                                    #[cfg(test)]
                                    if card_number == 0 {
                                        layout_probe.card = Some(card.response.rect);
                                    }
                                    paint_hazard_popup_card_border(
                                        ui.painter(),
                                        card.response.rect,
                                        style,
                                        &global,
                                        ui.clip_rect(),
                                    );
                                }
                                // ScrollArea's auto-sized viewport rounds a
                                // framed child a few pixels short in egui
                                // 0.34. Keep the card's bottom border inside
                                // the natural viewport without forcing a tall
                                // fixed-height empty body.
                                ui.add_space(4.0);
                            });
                        #[cfg(test)]
                        {
                            layout_probe.viewport = Some(_scroll.inner_rect);
                        }
                    });
            });

        // A short leader makes the popup's geographic attachment explicit
        // without covering the warning contents. It moves with the anchor as
        // the operator pans or zooms.
        if let Some((_, _, _, first_style)) = cards.first() {
            let card_rect = area.response.rect;
            let edge_x = if anchor.x < card_rect.center().x {
                card_rect.left()
            } else {
                card_rect.right()
            };
            let edge = egui::pos2(
                edge_x,
                anchor
                    .y
                    .clamp(card_rect.top() + 8.0, card_rect.bottom() - 8.0),
            );
            let color = style_color32(first_style.stroke_color);
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("hazard_map_popup"),
            ));
            painter.line_segment([anchor, edge], egui::Stroke::new(2.0_f32, color));
            painter.circle_filled(anchor, 3.0, color);
        }

        if let Some(popup) = self.hazard_map_popup.as_mut() {
            popup.screen_rect = Some(area.response.rect);
            #[cfg(test)]
            {
                popup.layout_probe = Some(layout_probe);
            }
        }
        if close_all {
            self.hazard_map_popup = None;
        }
        if let Some(index) = details_index {
            self.select_hazard_index(index);
            self.reveal_sidebar_section(sidebar_layout::SectionId::AlertsCurrent);
            self.sidebar_hidden = false;
        }
        ctx.request_repaint_after(Duration::from_secs(1));
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
            self.hazards_visible = true;
            self.persist_hazard_panel_settings();
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
        let metadata = self.cached_hazard_record_metadata();
        let frame_time = self.hazard_overlay_timeline_time();
        let visible_ids = overlay
            .records
            .iter()
            .enumerate()
            .filter(|(index, record)| {
                let record_metadata = metadata[*index];
                record_metadata.renderable
                    && self.hazard_record_visible_at_timeline_time_with_metadata(
                        record,
                        frame_time,
                        record_metadata,
                    )
            })
            .map(|(_, record)| record.event_id.clone())
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

    fn cached_hazard_record_metadata(&self) -> Arc<[HazardRecordRenderMetadata]> {
        let generation = self.hazard_overlay_generation;
        if let Some(metadata) = self
            .hazard_record_metadata_cache
            .borrow()
            .as_ref()
            .filter(|(cached_generation, _)| *cached_generation == generation)
            .map(|(_, metadata)| Arc::clone(metadata))
        {
            return metadata;
        }

        let metadata = self
            .hazard_overlay
            .as_ref()
            .map(|overlay| {
                overlay
                    .records
                    .iter()
                    .map(|record| {
                        let pds_watch = hazard_record_is_pds_watch(record);
                        HazardRecordRenderMetadata {
                            renderable: hazard_points_renderable(&record.points),
                            pds_watch,
                            watch_filter_key: hazard_watch_filter_key_with_pds(record, pds_watch),
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let metadata = Arc::<[HazardRecordRenderMetadata]>::from(metadata);
        *self.hazard_record_metadata_cache.borrow_mut() = Some((generation, Arc::clone(&metadata)));
        metadata
    }

    pub(crate) fn invalidate_hazard_record_metadata_cache(&self) {
        self.hazard_record_metadata_cache.borrow_mut().take();
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
        let pds_watch = hazard_record_is_pds_watch(record);
        self.hazard_record_visible_at_timeline_time_with_metadata(
            record,
            frame_time,
            HazardRecordRenderMetadata {
                // Visibility itself is independent from polygon validity; the
                // indexed paint/list paths consult the cached value separately.
                renderable: false,
                pds_watch,
                watch_filter_key: hazard_watch_filter_key_with_pds(record, pds_watch),
            },
        )
    }

    fn hazard_record_visible_at_timeline_time_with_metadata(
        &self,
        record: &HazardRecord,
        frame_time: Option<DateTime<Utc>>,
        metadata: HazardRecordRenderMetadata,
    ) -> bool {
        let pds_watch_visible_through_parent = metadata.pds_watch
            && !self
                .app_settings
                .hidden_hazard_watch_types
                .iter()
                .any(|hidden| hidden.eq_ignore_ascii_case("pds"));
        if self.hidden_hazard_families.contains(&record.event_family)
            && !pds_watch_visible_through_parent
        {
            return false;
        }
        if record.event_family == "watch"
            && self
                .app_settings
                .hidden_hazard_watch_types
                .iter()
                .any(|hidden| hidden.eq_ignore_ascii_case(metadata.watch_filter_key))
        {
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
        let metadata = self.cached_hazard_record_metadata();
        for (index, record) in overlay.records.iter().enumerate() {
            let record_metadata = metadata[index];
            if record_metadata.renderable
                && self.hazard_record_visible_at_timeline_time_with_metadata(
                    record,
                    frame_time,
                    record_metadata,
                )
            {
                index.hash(&mut hasher);
                record.event_id.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    #[cfg(test)]
    pub(crate) fn hazard_at_position(
        &self,
        rect: egui::Rect,
        position: egui::Pos2,
    ) -> Option<usize> {
        self.hazards_at_position(rect, position).into_iter().next()
    }

    /// All exact warning polygons beneath a map click, ordered by operational
    /// warning priority and then by smallest (most specific) polygon. Edge/
    /// label tolerance remains a single best hit: stacking several nearby-but-
    /// not-containing warnings is noisy and can imply the click was inside
    /// polygons it was not.
    pub(crate) fn hazards_at_position(&self, rect: egui::Rect, position: egui::Pos2) -> Vec<usize> {
        if !self.hazards_visible {
            return Vec::new();
        }
        let Some(overlay) = self.hazard_overlay.as_ref() else {
            return Vec::new();
        };
        let metadata = self.cached_hazard_record_metadata();
        let frame_time = self.hazard_overlay_timeline_time();
        let (lon, lat) = self.screen_to_lon_lat(rect, position);
        let point = HazardPoint { lon, lat };
        let mut containing = Vec::<(usize, f32, String)>::new();
        let mut best_near = None::<(usize, f32, f32, u8)>;
        let mut best_label = None::<(usize, f32, f32, u8)>;
        let mut seen_exact = BTreeSet::<String>::new();
        for (index, record) in overlay.records.iter().enumerate() {
            let record_metadata = metadata[index];
            if !record_metadata.renderable
                || !self.hazard_record_visible_at_timeline_time_with_metadata(
                    record,
                    frame_time,
                    record_metadata,
                )
            {
                continue;
            }
            let screen_area = self.hazard_screen_area(rect, &record.points);
            let family_order = hazard_family_order(&record.event_family);
            if bbox_contains(record.bbox, point.lon, point.lat)
                && hazard_polygon_contains_point(&record.points, point)
            {
                let base_id = base_hazard_event_id(&record.event_id).to_owned();
                if seen_exact.insert(base_id.clone()) {
                    containing.push((index, screen_area, base_id));
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
        if !containing.is_empty() {
            containing.sort_by(|left, right| {
                compare_hazard_popup_records(&overlay.records[left.0], &overlay.records[right.0])
                    .then_with(|| left.1.total_cmp(&right.1))
                    .then_with(|| left.2.cmp(&right.2))
            });
            return containing.into_iter().map(|(index, _, _)| index).collect();
        }
        best_near
            .map(|(index, _, _, _)| vec![index])
            .or_else(|| best_label.map(|(index, _, _, _)| vec![index]))
            .unwrap_or_default()
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

    /// Resolve warning geometry once at the start of a pane's paint pass. The
    /// returned Arc is then shared by the below-radar fills and above-radar
    /// outlines/labels without a second visibility-signature/cache lookup.
    pub(crate) fn hazard_overlay_shapes_for_draw(
        &self,
        rect: egui::Rect,
        fills_enabled: bool,
    ) -> Option<Arc<HazardOverlayShapes>> {
        if !self.hazards_visible {
            return None;
        }
        self.hazard_overlay.as_ref()?;
        Some(self.cached_hazard_overlay_shapes_with_fills(rect, fills_enabled))
    }

    pub(crate) fn draw_hazard_fills(
        &self,
        painter: &egui::Painter,
        built: Option<&HazardOverlayShapes>,
    ) {
        let Some(built) = built else {
            return;
        };
        painter.extend(built.fill_shapes.iter().cloned());
    }

    pub(crate) fn draw_hazard_overlays(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        built: Option<&HazardOverlayShapes>,
    ) {
        let Some(built) = built else {
            return;
        };
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
        let stroke = egui::Stroke::new(if blink_on { 4.5_f32 } else { 2.75_f32 }, color);
        let bounds = self.visible_geo_bounds(rect).expand(0.05);
        let metadata = self.cached_hazard_record_metadata();
        let frame_time = self.hazard_overlay_timeline_time();
        for (index, record) in overlay.records.iter().enumerate() {
            let record_metadata = metadata[index];
            if !self
                .unacknowledged_hazard_event_ids
                .contains(&record.event_id)
                || !record_metadata.renderable
                || !self.hazard_record_visible_at_timeline_time_with_metadata(
                    record,
                    frame_time,
                    record_metadata,
                )
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

    /// Fills-on shorthand. Production paints go through
    /// `cached_hazard_overlay_shapes_with_fills`, which defers the union while
    /// the map is dragged; this wrapper keeps the settled-frame default for
    /// tests that do not exercise the deferral.
    #[cfg(test)]
    pub(crate) fn cached_hazard_overlay_shapes(
        &self,
        rect: egui::Rect,
    ) -> Arc<HazardOverlayShapes> {
        self.cached_hazard_overlay_shapes_with_fills(rect, true)
    }

    pub(crate) fn cached_hazard_overlay_shapes_with_fills(
        &self,
        rect: egui::Rect,
        fills_enabled: bool,
    ) -> Arc<HazardOverlayShapes> {
        // Polygon projection + ear-clip tessellation is cached per view key:
        // idle repaints reuse it; pan/zoom/selection/content changes rebuild.
        // The generation counter invalidates exactly on overlay replacement.
        use std::hash::{Hash, Hasher};
        let frame_time = self.hazard_overlay_timeline_time();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.view_shape_key(2, rect).hash(&mut hasher);
        // Drag frames build outline-only; the settled frame must not reuse
        // that entry, so the flag is part of the identity.
        fills_enabled.hash(&mut hasher);
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
        for watch_type in &self.app_settings.hidden_hazard_watch_types {
            watch_type.hash(&mut hasher);
        }
        let key = hasher.finish();
        let mut cache = self.hazard_shape_cache.borrow_mut();
        cache
            .get_or_insert_with_weight(
                key,
                HAZARD_SHAPE_CACHE_GEOMETRY_BUDGET,
                || {
                    Arc::new(self.build_hazard_overlay_shapes_with_fills(
                        rect,
                        frame_time,
                        fills_enabled,
                    ))
                },
                |overlay| hazard_overlay_geometry_weight(overlay.as_ref()),
            )
            .clone()
    }

    /// Fills-on shorthand for tests; see `cached_hazard_overlay_shapes`.
    #[cfg(test)]
    pub(crate) fn build_hazard_overlay_shapes(
        &self,
        rect: egui::Rect,
        frame_time: Option<DateTime<Utc>>,
    ) -> HazardOverlayShapes {
        self.build_hazard_overlay_shapes_with_fills(rect, frame_time, true)
    }

    /// `fills_enabled == false` skips fill collection and the same-family
    /// scanline union entirely, keeping outlines and labels. The exact union
    /// is sweep- and work-budget-bounded, but remains the most expensive part
    /// of dense far-zoom warning geometry, so it is deferred while the map is
    /// under the pointer and paid once on the settled frame.
    pub(crate) fn build_hazard_overlay_shapes_with_fills(
        &self,
        rect: egui::Rect,
        frame_time: Option<DateTime<Utc>>,
        fills_enabled: bool,
    ) -> HazardOverlayShapes {
        let mut out = HazardOverlayShapes {
            fill_shapes: Vec::new(),
            outline_shapes: Vec::new(),
            labels: Vec::new(),
        };
        let Some(overlay) = &self.hazard_overlay else {
            return out;
        };
        let metadata = self.cached_hazard_record_metadata();
        let bounds = self.visible_geo_bounds(rect).expand(0.05);
        let mut visible_family_counts = HashMap::<&str, usize>::new();
        for (index, record) in overlay.records.iter().enumerate() {
            let record_metadata = metadata[index];
            if record_metadata.renderable
                && self.hazard_record_visible_at_timeline_time_with_metadata(
                    record,
                    frame_time,
                    record_metadata,
                )
                && bounds.intersects_bbox(record.bbox)
            {
                *visible_family_counts
                    .entry(record.event_family.as_str())
                    .or_default() += 1;
            }
        }
        let heavy_layer =
            visible_family_counts.values().sum::<usize>() > HAZARD_HEAVY_LAYER_LABEL_LIMIT;
        // Labels remain suppressed in a dense scene, but fills never switch
        // off at a record-count threshold. Each family instead shares a fixed
        // vertex budget, so zooming changes fill detail smoothly rather than
        // crossing an outline-only/fill-and-stall cliff.
        let mut label_rects = Vec::<egui::Rect>::new();
        let mut labeled_events = BTreeSet::<String>::new();
        let mut fill_candidates = Vec::<HazardFillCandidate>::new();
        for (index, record) in overlay.records.iter().enumerate() {
            let record_metadata = metadata[index];
            if !record_metadata.renderable
                || !self.hazard_record_visible_at_timeline_time_with_metadata(
                    record,
                    frame_time,
                    record_metadata,
                )
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
            // The geographic bounds test above is deliberately generous for
            // curved map projections. After projection, discard records whose
            // screen bbox is definitely outside the pane before they enter
            // simplification and the exact same-family union. This preserves
            // visible fills while keeping off-pane zone polygons out of the
            // expensive dense-warning path during pan and zoom.
            if !screen_polygon_bbox_intersects(&points, rect.expand(HAZARD_LABEL_CLICK_RADIUS_PX)) {
                continue;
            }
            let selected = self.selected_hazard_index == Some(index);
            let family_count = visible_family_counts
                .get(record.event_family.as_str())
                .copied()
                .unwrap_or(1);
            let style = self.style_registry.hazard_polygon(
                &record.event_family,
                hazard_record_style_threat_with_pds(record, record_metadata.pds_watch),
            );
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
            // A coastline-traced zone can carry thousands of vertices. Keep a
            // larger detail budget for the jump-safe outline and a smaller
            // budget for fill tessellation. Both budgets are family-wide, so
            // dense scenes remain bounded across pan/zoom without an
            // outline-only/fill-on threshold.
            let outline_points = if has_screen_jump {
                points.clone()
            } else {
                bounded_hazard_fill_points(&points, hazard_outline_vertex_limit(family_count))
                    .unwrap_or_else(|| points.clone())
            };
            // Alpha zero is a rendering disable, not merely transparent ink.
            // Avoid all tessellation/union work when there is nothing to
            // paint; this also makes the user-facing Fill=0 setting an actual
            // performance escape hatch.
            if fills_enabled
                && fill_alpha > 0
                && !has_screen_jump
                && let Some(fill_points) = bounded_hazard_fill_points(
                    &outline_points,
                    hazard_fill_vertex_limit(family_count),
                )
            {
                // Fills are not pushed directly: same-family same-color fills
                // are flattened after the loop so overlaps paint once.
                // Selection is shown by the boosted outline and deliberately
                // stays in this family union instead of splitting its fill.
                fill_candidates.push(HazardFillCandidate {
                    family: record.event_family.clone(),
                    fill,
                    points: fill_points,
                });
            }
            if solid {
                push_solid_closed_line(
                    &mut out.outline_shapes,
                    &outline_points,
                    stroke,
                    rect,
                    legit_px,
                );
            } else {
                match style.dash {
                    styles::DashPattern::Solid => unreachable!("solid handled above"),
                    styles::DashPattern::Dashed { dash, gap } => {
                        push_dashed_closed_line(
                            &mut out.outline_shapes,
                            &outline_points,
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
                            &outline_points,
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct HazardPopupLayout {
    position: egui::Pos2,
    width: f32,
    body_height: f32,
}

fn hazard_popup_layout(
    map_rect: egui::Rect,
    anchor: egui::Pos2,
    card_count: usize,
) -> HazardPopupLayout {
    let margin = 8.0;
    let gap = 14.0;
    let width = 300.0_f32.min((map_rect.width() - margin * 2.0).max(150.0));
    let available_height = (map_rect.height() - margin * 2.0).max(92.0);
    let body_height = (card_count.max(1) as f32 * 132.0).min((available_height - 42.0).max(52.0));
    let estimated_height = body_height + 42.0;
    let preferred_x = if anchor.x + gap + width <= map_rect.right() - margin {
        anchor.x + gap
    } else {
        anchor.x - gap - width
    };
    let max_x = (map_rect.right() - margin - width).max(map_rect.left() + margin);
    let x = preferred_x.clamp(map_rect.left() + margin, max_x);
    let max_y = (map_rect.bottom() - margin - estimated_height).max(map_rect.top() + margin);
    let y = (anchor.y - estimated_height * 0.5).clamp(map_rect.top() + margin, max_y);
    HazardPopupLayout {
        position: egui::pos2(x, y),
        width,
        body_height,
    }
}

fn hazard_popup_card_fill(
    panel: egui::Color32,
    fill_rgb: styles::Rgba,
    resolved_alpha: u8,
) -> egui::Color32 {
    // Map fills may intentionally be faint. A solid card surface keeps the
    // exact resolved/custom RGB while bounding its blend for readable text.
    let mix = (resolved_alpha as f32 / 255.0).clamp(0.24, 0.44);
    let channel =
        |base: u8, overlay: u8| (base as f32 * (1.0 - mix) + overlay as f32 * mix).round() as u8;
    egui::Color32::from_rgb(
        channel(panel.r(), fill_rgb[0]),
        channel(panel.g(), fill_rgb[1]),
        channel(panel.b(), fill_rgb[2]),
    )
}

fn hazard_popup_title(record: &HazardRecord) -> String {
    let canonical = match record.event_family.as_str() {
        "tornado" => Some("Tornado Warning"),
        "severe thunderstorm" => Some("Severe Thunderstorm Warning"),
        "flash flood" => Some("Flash Flood Warning"),
        "flood" => Some("Flood Warning"),
        "special marine" => Some("Special Marine Warning"),
        "snow squall" => Some("Snow Squall Warning"),
        "fire weather" => Some("Fire Weather Warning"),
        "special weather" => Some("Special Weather Statement"),
        "watch" => match hazard_record_style_threat(record) {
            Some("pds") => match hazard_watch_base_type(record) {
                Some("tornado") => Some("PDS Tornado Watch"),
                Some("severe-thunderstorm") => Some("PDS Severe Thunderstorm Watch"),
                _ => Some("PDS Watch"),
            },
            Some("tornado") => Some("Tornado Watch"),
            Some("severe-thunderstorm") => Some("Severe Thunderstorm Watch"),
            _ => None,
        },
        _ => None,
    };
    if let Some(canonical) = canonical {
        return canonical.to_owned();
    }
    if let Some(headline) = record
        .headline
        .as_deref()
        .map(str::trim)
        .filter(|headline| !headline.is_empty())
    {
        let first_line = headline.lines().next().unwrap_or(headline).trim();
        if first_line.chars().count() <= 70 {
            return first_line.to_owned();
        }
    }
    let family = title_case_tag(&record.event_family);
    if record.event_family == "watch" {
        return family;
    }
    if family.to_ascii_lowercase().contains("warning") {
        family
    } else {
        format!("{family} warning")
    }
}

fn hazard_popup_metric_lines(record: &HazardRecord) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(wind) = record.wind_mph.filter(|wind| *wind > 0) {
        lines.push(format!("Wind: {wind} mph"));
    }
    if let Some(hail) = record
        .hail_inches
        .filter(|hail| hail.is_finite() && *hail > 0.0)
    {
        lines.push(format!("Hail: {hail:.2} in"));
    }
    if let Some(tornado) = record
        .tornado
        .as_deref()
        .and_then(|tag| normalized_tornado_tag(tag, record.event_family.as_str()))
    {
        lines.push(tornado);
    }
    if let Some(tag) = record
        .damage_threat
        .as_deref()
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
    {
        lines.push(format!("Tag: {}", title_case_tag(tag)));
    }
    lines
}

fn normalized_tornado_tag(raw: &str, event_family: &str) -> Option<String> {
    let normalized = raw
        .trim()
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase();
    match normalized.as_str() {
        "" | "NONE" | "NO" => None,
        "POSSIBLE" => Some("Tornado possible".to_owned()),
        "RADAR INDICATED" if event_family == "tornado" => Some("Radar indicated".to_owned()),
        "RADAR INDICATED" => Some("Tornado possible · radar indicated".to_owned()),
        "OBSERVED" => Some("Tornado observed".to_owned()),
        _ => Some(format!("Tornado: {}", title_case_tag(&normalized))),
    }
}

fn title_case_tag(raw: &str) -> String {
    raw.split_whitespace()
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            let mut chars = lower.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn hazard_popup_office(record: &HazardRecord) -> String {
    let base_id = base_hazard_event_id(&record.event_id);
    let vtec = base_id.split('.').next().unwrap_or_default().trim();
    if vtec.len() == 4 && vtec.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return vtec.to_ascii_uppercase();
    }
    record.office.trim().to_owned()
}

fn hazard_popup_expiry_text(
    record: &HazardRecord,
    now: DateTime<Utc>,
    archive_timeline: bool,
    time_zone: DisplayTimeZone,
) -> String {
    let Some(end) = parse_hazard_record_time(&record.valid_end) else {
        return "Valid time unavailable".to_owned();
    };
    if archive_timeline {
        return format!("Valid until {}", time_zone.format_date_hm(end));
    }
    let clock = time_zone.format_hm(end);
    let seconds = (end - now).num_seconds();
    if seconds >= 0 {
        let minutes = (seconds + 59) / 60;
        if minutes >= 60 {
            format!(
                "Expires in {}h {:02}m · {clock}",
                minutes / 60,
                minutes % 60
            )
        } else {
            format!("Expires in {minutes} min · {clock}")
        }
    } else {
        let minutes = ((-seconds) + 59) / 60;
        format!("Expired {minutes} min ago · {clock}")
    }
}

fn paint_hazard_popup_card_border(
    painter: &egui::Painter,
    rect: egui::Rect,
    style: &styles::PolygonStyle,
    global: &styles::HazardGlobalStyle,
    clip_rect: egui::Rect,
) {
    let base = style_color32(style.stroke_color);
    let color = egui::Color32::from_rgba_unmultiplied(
        base.r(),
        base.g(),
        base.b(),
        global.stroke_alpha_selected.max(180),
    );
    let stroke = egui::Stroke::new(
        (style.stroke_width * global.stroke_width_scale).max(1.5),
        color,
    );
    let inset = rect.shrink(stroke.width * 0.5);
    match style.dash {
        styles::DashPattern::Solid => {
            painter.rect_stroke(
                inset,
                f32::from(HAZARD_POPUP_CARD_RADIUS),
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        dash => {
            // Follow the card's rounded silhouette. The old square path met
            // the inset top accent at different coordinates, leaving the
            // dashed/dotted side rails visibly short of the top edge.
            let points = hazard_popup_card_outline(rect, stroke.width);
            let mut shapes = Vec::new();
            match dash {
                styles::DashPattern::Solid => unreachable!("solid handled above"),
                styles::DashPattern::Dashed { dash, gap } => push_dashed_closed_line(
                    &mut shapes,
                    &points,
                    stroke,
                    dash,
                    gap,
                    clip_rect,
                    100_000.0,
                ),
                styles::DashPattern::Dotted => {
                    let dot = stroke.width.max(1.0);
                    push_dashed_closed_line(
                        &mut shapes,
                        &points,
                        stroke,
                        dot,
                        dot * 2.0,
                        clip_rect,
                        100_000.0,
                    );
                }
            }
            painter.extend(shapes);
        }
    }
    if let Some(accent) = hazard_popup_card_top_accent(rect, stroke.width) {
        painter.line_segment(accent, egui::Stroke::new(3.0_f32, color));
    }
}

const HAZARD_POPUP_CARD_RADIUS: u8 = 4;

/// Keep the emphasized top rule inside the card's rounded silhouette.
///
/// A rectangular strip spanning `rect.left()..=rect.right()` protrudes at the
/// two top corners because the card itself does not occupy those pixels.  The
/// rule therefore runs only between the rounded-rectangle tangent points.
fn hazard_popup_card_top_accent(rect: egui::Rect, stroke_width: f32) -> Option<[egui::Pos2; 2]> {
    let inset = rect.shrink(stroke_width.max(0.0) * 0.5);
    let radius = (f32::from(HAZARD_POPUP_CARD_RADIUS) - stroke_width.max(0.0) * 0.5)
        .max(0.0)
        .min(inset.width() * 0.5)
        .min(inset.height() * 0.5);
    if inset.width() <= radius * 2.0 || inset.height() <= 0.0 {
        return None;
    }
    Some([
        egui::pos2(inset.left() + radius, inset.top()),
        egui::pos2(inset.right() - radius, inset.top()),
    ])
}

/// Clockwise rounded-rectangle centerline used by dashed/dotted warning
/// cards. Its first two points are exactly the top-accent endpoints, so the
/// side outline and emphasized top rule share one continuous geometry.
fn hazard_popup_card_outline(rect: egui::Rect, stroke_width: f32) -> Vec<egui::Pos2> {
    let inset = rect.shrink(stroke_width.max(0.0) * 0.5);
    if inset.width() <= 0.0 || inset.height() <= 0.0 {
        return Vec::new();
    }
    let radius = (f32::from(HAZARD_POPUP_CARD_RADIUS) - stroke_width.max(0.0) * 0.5)
        .max(0.0)
        .min(inset.width() * 0.5)
        .min(inset.height() * 0.5);
    if radius <= f32::EPSILON {
        return vec![
            inset.left_top(),
            inset.right_top(),
            inset.right_bottom(),
            inset.left_bottom(),
        ];
    }

    const ARC_STEPS: usize = 4;
    let mut points = Vec::with_capacity(ARC_STEPS * 4 + 4);

    points.push(egui::pos2(inset.left() + radius, inset.top()));
    points.push(egui::pos2(inset.right() - radius, inset.top()));
    push_hazard_popup_card_arc(
        &mut points,
        egui::pos2(inset.right() - radius, inset.top() + radius),
        -std::f32::consts::FRAC_PI_2,
        0.0,
        radius,
        ARC_STEPS,
        true,
    );
    points.push(egui::pos2(inset.right(), inset.bottom() - radius));
    push_hazard_popup_card_arc(
        &mut points,
        egui::pos2(inset.right() - radius, inset.bottom() - radius),
        0.0,
        std::f32::consts::FRAC_PI_2,
        radius,
        ARC_STEPS,
        true,
    );
    points.push(egui::pos2(inset.left() + radius, inset.bottom()));
    push_hazard_popup_card_arc(
        &mut points,
        egui::pos2(inset.left() + radius, inset.bottom() - radius),
        std::f32::consts::FRAC_PI_2,
        std::f32::consts::PI,
        radius,
        ARC_STEPS,
        true,
    );
    points.push(egui::pos2(inset.left(), inset.top() + radius));
    push_hazard_popup_card_arc(
        &mut points,
        egui::pos2(inset.left() + radius, inset.top() + radius),
        std::f32::consts::PI,
        std::f32::consts::PI * 1.5,
        radius,
        ARC_STEPS,
        false,
    );
    points
}

fn push_hazard_popup_card_arc(
    points: &mut Vec<egui::Pos2>,
    center: egui::Pos2,
    start: f32,
    end: f32,
    radius: f32,
    steps: usize,
    include_end: bool,
) {
    let last = if include_end { steps + 1 } else { steps };
    for step in 1..last {
        let fraction = step as f32 / steps as f32;
        let angle = start + (end - start) * fraction;
        points.push(center + egui::vec2(angle.cos(), angle.sin()) * radius);
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
    fn hazard_geometry_weight_recurses_through_nested_shape_vectors() {
        let path = egui::Shape::line(
            vec![
                egui::pos2(0.0, 0.0),
                egui::pos2(1.0, 1.0),
                egui::pos2(2.0, 0.0),
            ],
            egui::Stroke::new(1.0, egui::Color32::WHITE),
        );
        let path_weight = egui_shape_geometry_weight(&path);
        assert_eq!(
            path_weight,
            std::mem::size_of::<egui::Shape>() + 3 * std::mem::size_of::<egui::Pos2>()
        );

        let nested = egui::Shape::Vec(vec![
            egui::Shape::Noop,
            egui::Shape::Vec(vec![path.clone()]),
        ]);
        assert_eq!(
            egui_shape_geometry_weight(&nested),
            path_weight + 3 * std::mem::size_of::<egui::Shape>()
        );

        let overlay = HazardOverlayShapes {
            fill_shapes: vec![nested],
            outline_shapes: vec![path],
            labels: vec![(egui::Pos2::ZERO, "TOR 0042".to_owned(), false, 0)],
        };
        assert!(
            hazard_overlay_geometry_weight(&overlay) > 2 * path_weight + "TOR 0042".len(),
            "top-level shape and label containers must also count"
        );
    }

    fn popup_test_record(
        event_id: &str,
        family: &str,
        west: f32,
        south: f32,
        east: f32,
        north: f32,
    ) -> HazardRecord {
        HazardRecord {
            event_id: event_id.to_owned(),
            label: event_id.to_owned(),
            event_family: family.to_owned(),
            action: "NEW".to_owned(),
            lifecycle_status: Some("Active".to_owned()),
            office: "NWS Test".to_owned(),
            headline: None,
            source_url: None,
            area: None,
            motion: None,
            details: Vec::new(),
            valid_start: Some("2026-07-15T19:00:00Z".to_owned()),
            valid_end: Some("2026-07-15T20:30:00Z".to_owned()),
            severity: None,
            certainty: None,
            urgency: None,
            tornado: None,
            hail_inches: None,
            wind_mph: None,
            damage_threat: None,
            points: vec![
                HazardPoint {
                    lon: west,
                    lat: south,
                },
                HazardPoint {
                    lon: east,
                    lat: south,
                },
                HazardPoint {
                    lon: east,
                    lat: north,
                },
                HazardPoint {
                    lon: west,
                    lat: north,
                },
            ],
            bbox: [west, south, east, north],
        }
    }

    #[test]
    fn alert_qol_panel_defaults_copy_into_persisted_settings() {
        let mut app = crate::tests::test_viewer_app_with_hazards(Vec::new());
        app.hazards_visible = false;
        app.hazards_active_only = false;
        app.live_hazard_auto_refresh = false;
        app.hidden_hazard_families.clear();
        app.hidden_hazard_families.insert("tornado".to_owned());

        app.persist_hazard_panel_settings();

        assert!(!app.app_settings.hazards_visible);
        assert!(!app.app_settings.hazards_active_only);
        assert!(!app.app_settings.live_hazard_auto_refresh);
        assert_eq!(
            app.app_settings.hidden_hazard_families,
            vec!["tornado".to_owned()]
        );
    }

    #[test]
    fn exact_overlap_stacks_all_unique_events_priority_then_area() {
        let broad = popup_test_record(
            "KTLX.SV.W.0001",
            "severe thunderstorm",
            -2.0,
            -2.0,
            2.0,
            2.0,
        );
        let narrow = popup_test_record("KTLX.TO.W.0002", "tornado", -0.5, -0.5, 0.5, 0.5);
        let duplicate_part = popup_test_record("KTLX.TO.W.0002#1", "tornado", -0.4, -0.4, 0.4, 0.4);
        let mut app =
            crate::tests::test_viewer_app_with_hazards(vec![broad, narrow, duplicate_part]);
        app.hazards_active_only = false;
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));

        let hits = app.hazards_at_position(rect, rect.center());

        assert_eq!(hits, vec![1, 0], "multipart pieces must dedupe by base id");
    }

    #[test]
    fn exact_overlap_orders_operational_priority_before_polygon_area() {
        let watch = popup_test_record("KOUN.SV.A.0500", "watch", -0.1, -0.1, 0.1, 0.1);
        let ordinary = popup_test_record("KOUN.FF.W.0501", "flash flood", -0.2, -0.2, 0.2, 0.2);
        let mut escalated = popup_test_record(
            "KOUN.SV.W.0502",
            "severe thunderstorm",
            -0.5,
            -0.5,
            0.5,
            0.5,
        );
        escalated.damage_threat = Some("DESTRUCTIVE".to_owned());
        let tornado = popup_test_record("KOUN.TO.W.0503", "tornado", -1.0, -1.0, 1.0, 1.0);
        let mut app =
            crate::tests::test_viewer_app_with_hazards(vec![watch, ordinary, escalated, tornado]);
        app.hazards_active_only = false;
        app.hidden_hazard_families.remove("watch");
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));

        assert_eq!(
            app.hazards_at_position(rect, rect.center()),
            vec![3, 2, 1, 0],
            "TOR > escalated SVR > ordinary warning > watch must beat area"
        );
    }

    #[test]
    fn pds_watch_bypasses_legacy_hidden_watch_parent_only_until_explicitly_hidden() {
        let mut pds = popup_test_record("KOUN.TO.A.0504", "watch", -1.0, -1.0, 1.0, 1.0);
        pds.details = vec!["THIS IS A PARTICULARLY DANGEROUS SITUATION".to_owned()];
        let ordinary = popup_test_record("KOUN.TO.A.0505", "watch", -1.0, -1.0, 1.0, 1.0);
        let mut app = crate::tests::test_viewer_app_with_hazards(vec![pds, ordinary]);
        app.hidden_hazard_families.insert("watch".to_owned());

        assert!(app.hazard_record_visible(&app.hazard_overlay.as_ref().unwrap().records[0]));
        assert!(!app.hazard_record_visible(&app.hazard_overlay.as_ref().unwrap().records[1]));

        app.app_settings
            .hidden_hazard_watch_types
            .push("pds".to_owned());
        assert!(!app.hazard_record_visible(&app.hazard_overlay.as_ref().unwrap().records[0]));
    }

    #[test]
    fn installed_hazard_metadata_reuses_and_invalidates_on_overlay_generation() {
        let mut pds = popup_test_record("KOUN.TO.A.0504", "watch", -1.0, -1.0, 1.0, 1.0);
        pds.details = vec!["THIS IS A PARTICULARLY DANGEROUS SITUATION".to_owned()];
        let mut app = crate::tests::test_viewer_app_with_hazards(vec![pds]);

        let first = app.cached_hazard_record_metadata();
        let reused = app.cached_hazard_record_metadata();
        assert!(Arc::ptr_eq(&first, &reused));
        assert!(first[0].renderable);
        assert!(first[0].pds_watch);
        assert_eq!(first[0].watch_filter_key, "pds");

        let record = &mut app.hazard_overlay.as_mut().unwrap().records[0];
        record.details.clear();
        record.points.truncate(2);
        app.hazard_overlay_generation = app.hazard_overlay_generation.wrapping_add(1);

        let refreshed = app.cached_hazard_record_metadata();
        assert!(!Arc::ptr_eq(&first, &refreshed));
        assert!(!refreshed[0].renderable);
        assert!(!refreshed[0].pds_watch);
        assert_eq!(refreshed[0].watch_filter_key, "tornado");
    }

    #[test]
    fn pane_draw_preparation_reuses_one_shape_arc_and_honors_visibility_gate() {
        let warning = popup_test_record("KOUN.TO.W.0504", "tornado", -101.0, 34.0, -100.0, 35.0);
        let mut app = crate::tests::test_viewer_app_with_hazards(vec![warning]);
        app.hazards_active_only = false;
        app.map_center_lon = -100.5;
        app.map_center_lat = 34.5;
        app.map_scale = 360.0;
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));

        let prepared = app.hazard_overlay_shapes_for_draw(rect, true).unwrap();
        let same_pane_cache_entry = app.cached_hazard_overlay_shapes(rect);
        assert!(Arc::ptr_eq(&prepared, &same_pane_cache_entry));

        app.hazards_visible = false;
        assert!(app.hazard_overlay_shapes_for_draw(rect, true).is_none());
    }

    #[test]
    fn map_popup_stores_stable_event_ids_and_geographic_anchor() {
        let warning = popup_test_record(
            "KTLX.SV.W.0001#0",
            "severe thunderstorm",
            -1.0,
            -1.0,
            1.0,
            1.0,
        );
        let mut app = crate::tests::test_viewer_app_with_hazards(vec![warning]);
        app.hazards_active_only = false;
        app.grid_layout = PanelLayout::TwoVertical;
        app.active_pane = 1;
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let pointer = rect.center();
        let expected = app.screen_to_lon_lat(rect, pointer);

        assert!(app.open_hazard_popup_from_map(rect, pointer));
        let popup = app.hazard_map_popup.as_ref().expect("warning stack opens");
        assert_eq!(popup.event_ids, vec!["KTLX.SV.W.0001"]);
        assert_eq!(
            popup.pane_index, 1,
            "popup must retain its owning grid pane"
        );
        assert!((popup.anchor.lon - expected.0).abs() < 0.001);
        assert!((popup.anchor.lat - expected.1).abs() < 0.001);
    }

    #[test]
    fn warning_card_metrics_suppress_zeroes_and_normalize_tags() {
        let mut record = popup_test_record(
            "KTLX.SV.W.0001",
            "severe thunderstorm",
            -1.0,
            -1.0,
            1.0,
            1.0,
        );
        record.wind_mph = Some(0);
        record.hail_inches = Some(0.0);
        record.tornado = Some("RADAR_INDICATED".to_owned());
        record.damage_threat = Some("DESTRUCTIVE".to_owned());

        assert_eq!(
            hazard_popup_metric_lines(&record),
            vec![
                "Tornado possible · radar indicated".to_owned(),
                "Tag: Destructive".to_owned()
            ]
        );

        record.wind_mph = Some(80);
        record.hail_inches = Some(2.0);
        let metrics = hazard_popup_metric_lines(&record);
        assert!(metrics.contains(&"Wind: 80 mph".to_owned()));
        assert!(metrics.contains(&"Hail: 2.00 in".to_owned()));
    }

    #[test]
    fn overlapping_tornado_and_severe_cards_keep_detection_phrasing_separate() {
        let mut tornado = popup_test_record("KGRB.TO.W.0033", "tornado", -1.0, -1.0, 1.0, 1.0);
        tornado.tornado = Some("RADAR INDICATED".to_owned());
        let mut severe = popup_test_record(
            "KGRB.SV.W.0034",
            "severe thunderstorm",
            -1.0,
            -1.0,
            1.0,
            1.0,
        );
        severe.tornado = Some("RADAR INDICATED".to_owned());

        assert_eq!(hazard_popup_metric_lines(&tornado), vec!["Radar indicated"]);
        assert_eq!(
            hazard_popup_metric_lines(&severe),
            vec!["Tornado possible · radar indicated"]
        );
    }

    #[test]
    fn popup_office_prefers_vtec_letters_and_falls_back_to_sender() {
        let mut vtec = popup_test_record(
            "KSGF.SV.W.0042",
            "severe thunderstorm",
            -1.0,
            -1.0,
            1.0,
            1.0,
        );
        vtec.office = "NWS Springfield MO".to_owned();
        assert_eq!(hazard_popup_office(&vtec), "KSGF");

        vtec.event_id = "2026-europe-alert".to_owned();
        assert_eq!(hazard_popup_office(&vtec), "NWS Springfield MO");
    }

    #[test]
    fn expiry_countdown_and_archive_clock_honor_display_time_zone() {
        let record = popup_test_record(
            "KTLX.SV.W.0001",
            "severe thunderstorm",
            -1.0,
            -1.0,
            1.0,
            1.0,
        );
        let now = Utc.with_ymd_and_hms(2026, 7, 15, 20, 16, 0).unwrap();

        assert_eq!(
            hazard_popup_expiry_text(&record, now, false, DisplayTimeZone::Utc),
            "Expires in 14 min · 20:30Z"
        );
        assert_eq!(
            hazard_popup_expiry_text(&record, now, true, DisplayTimeZone::Utc),
            "Valid until 2026-07-15 20:30Z"
        );
        assert_eq!(
            hazard_popup_expiry_text(&record, now, false, DisplayTimeZone::Eastern),
            "Expires in 14 min · 16:30 EDT"
        );
        assert_eq!(
            hazard_popup_expiry_text(&record, now, true, DisplayTimeZone::Eastern),
            "Valid until 2026-07-15 16:30 EDT"
        );
    }

    #[test]
    fn popup_layout_stays_inside_map_and_caps_overlap_stack() {
        let rect = egui::Rect::from_min_max(egui::pos2(100.0, 50.0), egui::pos2(700.0, 450.0));
        let layout = hazard_popup_layout(rect, egui::pos2(695.0, 445.0), 12);
        let estimated = egui::Rect::from_min_size(
            layout.position,
            egui::vec2(layout.width, layout.body_height + 42.0),
        );
        assert!(rect.shrink(7.9).contains_rect(estimated));
        assert_eq!(layout.width, 300.0, "desktop popup must stay compact");
        assert!(
            layout.body_height < 12.0 * 132.0,
            "large stacks must scroll"
        );
    }

    #[test]
    fn popup_card_top_accent_stays_inside_rounded_corner_tangents() {
        let rect = egui::Rect::from_min_max(egui::pos2(9.0, 20.0), egui::pos2(292.0, 120.0));
        let stroke_width = 3.0;
        let [left, right] = hazard_popup_card_top_accent(rect, stroke_width)
            .expect("normal warning card has a top accent");
        let radius = f32::from(HAZARD_POPUP_CARD_RADIUS);

        assert_eq!(left.x, rect.left() + radius);
        assert_eq!(right.x, rect.right() - radius);
        assert_eq!(left.y, rect.top() + 1.5);
        assert_eq!(right.y, left.y);
        let outline = hazard_popup_card_outline(rect, stroke_width);
        assert_eq!(outline.first().copied(), Some(left));
        assert_eq!(outline.get(1).copied(), Some(right));
        assert!(
            outline.iter().all(|point| rect.contains(*point)),
            "rounded dashed outline must remain inside the card"
        );
        assert!(
            hazard_popup_card_top_accent(
                egui::Rect::from_min_size(rect.min, egui::vec2(radius * 2.0, 2.0),),
                stroke_width
            )
            .is_none()
        );
    }

    #[test]
    fn popup_title_uses_compact_event_name_instead_of_long_nws_headline() {
        let mut record = popup_test_record(
            "KDTX.SV.W.0042",
            "severe thunderstorm",
            -1.0,
            -1.0,
            1.0,
            1.0,
        );
        record.headline = Some(
            "Severe Thunderstorm Warning issued July 15 at 7:22PM EDT until July 15 at 7:45PM EDT by NWS Detroit/Pontiac MI"
                .to_owned(),
        );

        assert_eq!(hazard_popup_title(&record), "Severe Thunderstorm Warning");
    }

    #[test]
    fn popup_title_names_tornado_and_severe_thunderstorm_watches() {
        let tornado_watch = popup_test_record("KGRB.TO.A.0501", "watch", -1.0, -1.0, 1.0, 1.0);
        let severe_watch = popup_test_record("KOUN.SV.A.0502", "watch", -1.0, -1.0, 1.0, 1.0);

        assert_eq!(hazard_popup_title(&tornado_watch), "Tornado Watch");
        assert_eq!(
            hazard_popup_title(&severe_watch),
            "Severe Thunderstorm Watch"
        );
    }

    #[test]
    fn popup_title_names_pds_watch_and_preserves_base_kind() {
        let mut pds = popup_test_record("KOUN.TO.A.0503", "watch", -1.0, -1.0, 1.0, 1.0);
        pds.details = vec!["PARTICULARLY DANGEROUS SITUATION".to_owned()];

        assert_eq!(hazard_popup_title(&pds), "PDS Tornado Watch");
    }

    #[test]
    fn popup_title_never_calls_an_unknown_watch_a_warning() {
        let watch = popup_test_record("SPC.WW.0503", "watch", -1.0, -1.0, 1.0, 1.0);

        assert_eq!(hazard_popup_title(&watch), "Watch");
    }

    #[test]
    fn rendered_popup_is_compact_and_every_standard_field_is_visible() {
        let mut record = popup_test_record(
            "KDTX.SV.W.0042",
            "severe thunderstorm",
            -1.0,
            -1.0,
            1.0,
            1.0,
        );
        record.headline = Some(
            "Severe Thunderstorm Warning issued July 15 at 7:22PM EDT until July 15 at 7:45PM EDT by NWS Detroit/Pontiac MI"
                .to_owned(),
        );
        record.wind_mph = Some(60);
        record.hail_inches = Some(1.5);
        let mut app = crate::tests::test_viewer_app_with_hazards(vec![record]);
        app.hazards_active_only = false;
        let map_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        assert!(app.open_hazard_popup_from_map(map_rect, map_rect.center()));
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(map_rect),
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                app.show_hazard_map_popup(ui.ctx(), map_rect);
            });
        }
        let popup = app
            .hazard_map_popup
            .as_ref()
            .expect("popup remains open after rendering");
        let rendered = popup.screen_rect.expect("popup was rendered");
        assert!(
            rendered.width() <= 302.0,
            "popup expanded past compact bound: {rendered:?}"
        );
        let probe = popup
            .layout_probe
            .as_ref()
            .expect("test layout probe was recorded");
        let viewport = probe.viewport.expect("scroll viewport exists");
        let card = probe.card.expect("card frame exists");
        assert!(
            viewport.expand(0.5).contains_rect(card),
            "standard warning card is clipped by its viewport: viewport={viewport:?}, card={card:?}"
        );
        for (name, rect) in [
            ("title", probe.title),
            ("Wind", probe.wind),
            ("Hail", probe.hail),
            ("expiry/WFO", probe.footer),
            ("Details", probe.details),
        ] {
            let rect = rect.unwrap_or_else(|| panic!("{name} widget was not rendered"));
            assert!(
                rect.width() > 0.0 && rect.height() > 0.0,
                "{name}: {rect:?}"
            );
            assert!(
                card.expand(0.5).contains_rect(rect),
                "{name} escaped the card: card={card:?}, widget={rect:?}"
            );
        }
    }

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

    #[test]
    fn feedback_ux_current_alert_accent_flashes_only_until_acknowledged() {
        let family = egui::Color32::from_rgb(20, 180, 90);
        let flash_a = current_alert_accent_color(Some(family), true, 0.0);
        let flash_b = current_alert_accent_color(Some(family), true, 0.5);
        assert_ne!(flash_a, Some(family));
        assert_ne!(flash_b, Some(family));
        assert_ne!(flash_a, flash_b, "unacknowledged indicator must alternate");
        assert_eq!(
            current_alert_accent_color(Some(family), false, 0.0),
            Some(family),
            "acknowledgement restores the warning-family accent"
        );
        assert_eq!(current_alert_accent_color(None, false, 0.5), None);
    }
}
