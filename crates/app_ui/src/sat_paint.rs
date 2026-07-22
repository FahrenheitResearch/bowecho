//! Satellite window/panel UI, worker pump, and the satellite map layer
//! (install / LUT cache / poll / draw), plus the pure sat frame-selection
//! and grid-sampling helpers, moved verbatim out of `main.rs` (v0.30
//! decomposition, queue item #5). Sat types, consts, and `ViewerApp`
//! fields stay in `main.rs` (`SatMapSampleCtx`, `SatFrameResizeResult`,
//! `anchored_sat_texture_rect`, the timeline-follow methods); this module
//! reaches them via `crate::`.

use crate::*;

/// Provider control surface selected in the unified Satellite window. The
/// player below remains shared: every provider writes the same geolocated
/// store contract, so switching controls never discards stored loops.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SatelliteSource {
    #[default]
    Goes,
    Himawari,
    Meteosat,
}

impl SatelliteSource {
    pub(crate) const ALL: [Self; 3] = [Self::Goes, Self::Himawari, Self::Meteosat];

    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::Goes => "goes",
            Self::Himawari => "himawari",
            Self::Meteosat => "meteosat",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Goes => "GOES",
            Self::Himawari => "Himawari-9",
            Self::Meteosat => "Meteosat-12",
        }
    }

    pub(crate) fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "himawari" | "h9" => Self::Himawari,
            "meteosat" | "mtg" | "mtg-i1" => Self::Meteosat,
            _ => Self::Goes,
        }
    }
}

impl ViewerApp {
    /// Satellite window: GOES live-follow plus non-GOES ingest/discovery
    /// actions, all writing into BowEcho's own rolling satellite store.
    pub(crate) fn satellite_window(&mut self, ctx: &egui::Context) {
        if !self.show_satellite {
            return;
        }
        self.ensure_satellite_worker(ctx);
        if let Some(source) = self.pump_sat_responses(ctx) {
            self.open_satellite_native_plot(ctx, source);
        }
        self.maybe_sync_satellite_map_to_timeline(ctx);
        if self.workspace.is_docked(dock::WorkspacePane::Satellite) {
            return; // body renders as a workspace pane
        }
        let mut open = self.show_satellite;
        let mut events = (Vec::new(), Vec::new());
        egui::Window::new("Satellite")
            .id(egui::Id::new("bowecho_satellite_window"))
            .open(&mut open)
            .default_size([900.0, 700.0])
            .min_size([520.0, 400.0])
            .collapsible(false)
            .resizable(true)
            .show(ctx, |ui| {
                self.dock_toggle_row(ui, dock::WorkspacePane::Satellite);
                events = self.satellite_pane_body(ui);
            });
        self.set_viewer_open(dock::WorkspacePane::Satellite, open);
        let (panel_events, player_events) = events;
        self.handle_satellite_events(panel_events, player_events);
    }

    pub(crate) fn ensure_satellite_worker(&mut self, ctx: &egui::Context) {
        if self.sat.is_some() {
            return;
        }
        // BowEcho's OWN sat store: rw-sat's download cache + rolling store
        // have no cross-process locking, so sharing rusty-weather's
        // store/sat corrupts reads when both apps follow at once
        // (field failure: checksum mismatch on files rusty-weather
        // was mid-writing).
        let store = settings::sat_store_dir();
        let notify = ctx.clone();
        let worker = sat_worker::SatWorker::spawn(store, move || {
            notify.request_repaint();
        });
        self.sat_panel
            .set_satellite_options(sat_worker::satellite_options());
        self.sat_panel
            .set_sector_options(sat_worker::sector_options());
        self.sat_panel
            .set_layer_options(sat_worker::layer_options());
        worker.send(sat_worker::SatRequest::Validate(
            self.sat_panel.spec().clone(),
        ));
        worker.send(sat_worker::SatRequest::SetIrEnhancement(
            self.sat_ir_enhancement,
        ));
        if eumetsat_credentials::DATA_STORE_ACCOUNT_UI_ENABLED {
            worker.send(sat_worker::SatRequest::LoadEumetsatCredentials);
        }
        worker.send(sat_worker::SatRequest::Scan);
        self.sat = Some(worker);
    }

    /// The persisted native-resolution window, when enabled — handed to the
    /// true-color composite ingests so they fetch/compose just that box at
    /// full instrument resolution.
    fn sat_native_window(&self) -> Option<sat_window::SatNativeWindow> {
        self.sat_window_enabled.then(|| {
            sat_window::SatNativeWindow {
                center_lat_deg: self.sat_window_lat_deg,
                center_lon_deg: self.sat_window_lon_deg,
                size_km: f64::from(self.sat_window_size_km),
            }
            .clamped()
        })
    }

    /// The native window, but only when its center is plausibly within view
    /// of the satellite at `sub_lon_deg`. ONE window is persisted and both
    /// satellites' composite loads read it, so without this gate a saved
    /// Guam window hard-fails every GOES composite ("outside view") and a
    /// CONUS window kills every Himawari one. A dropped window leaves that
    /// load on its normal full-sector spec (the stride-1 native decode only
    /// engages when a window is actually attached), with one panel note so
    /// the user can tell.
    pub(crate) fn sat_native_window_if_visible(
        &mut self,
        sub_lon_deg: f64,
        satellite_label: &str,
    ) -> Option<sat_window::SatNativeWindow> {
        let window = self.sat_native_window()?;
        if sat_window::window_visible_from_sub_lon(sub_lon_deg, &window) {
            return Some(window);
        }
        self.sat_panel.apply_note(format!(
            "Native window {} is beyond {satellite_label}'s horizon — composing the normal full sector instead",
            window.run_slug()
        ));
        None
    }

    /// Persist the native-window controls (AppSettings microdegree fields).
    fn persist_sat_native_window(&mut self) {
        self.app_settings.sat_native_window_enabled = self.sat_window_enabled;
        self.app_settings.sat_native_window_lat_e6 = (self.sat_window_lat_deg * 1e6).round() as i64;
        self.app_settings.sat_native_window_lon_e6 = (self.sat_window_lon_deg * 1e6).round() as i64;
        self.app_settings.sat_native_window_size_km = self.sat_window_size_km;
        self.mark_app_settings_dirty();
    }

    /// Queue public EUMETView imagery through the same player/store/map path
    /// as GOES and Himawari. Exposed to the Layers > Lightning shortcut too.
    pub(crate) fn queue_meteosat_product(
        &mut self,
        ctx: &egui::Context,
        product: eumetsat::MtgProduct,
        frame_count: usize,
    ) {
        self.ensure_satellite_worker(ctx);
        self.satellite_source = SatelliteSource::Meteosat;
        self.eumetsat_product = product.slug().to_owned();
        self.app_settings.satellite_source = self.satellite_source.slug().to_owned();
        self.app_settings.eumetsat_product = self.eumetsat_product.clone();
        self.mark_app_settings_dirty();
        let window = self.sat_native_window_if_visible(0.0, "Meteosat-12");
        let scope = window
            .map(|window| format!(" · focused {}", window.run_slug()))
            .unwrap_or_default();
        self.sat_map_follow = true;
        self.status = format!(
            "Satellite: loading Meteosat-12 {} · {} frame{}{}",
            product.label(),
            frame_count,
            if frame_count == 1 { "" } else { "s" },
            scope
        );
        self.sat_panel.apply_note(format!(
            "Meteosat: queued {} {} frame{}{}",
            product.label(),
            frame_count,
            if frame_count == 1 { "" } else { "s" },
            scope
        ));
        if let Some(sat) = &self.sat {
            sat.send(sat_worker::SatRequest::IngestMeteosatWms(
                sat_worker::MeteosatWmsSpec {
                    product: product.slug().to_owned(),
                    frame_count: frame_count.clamp(1, 36),
                    window,
                    max_image_edge: if window.is_some() { 2_048 } else { 1_600 },
                },
            ));
        }
    }

    /// Provider-first control surface. All provider actions feed the shared
    /// region controls and the player/output section below it.
    fn satellite_provider_controls(&mut self, ui: &mut egui::Ui) -> Vec<rw_ui::SatelliteEvent> {
        panel_kit::subgroup(ui, "Source & product", |_ui| {});
        let spacing = ui.spacing().item_spacing.x;
        let tab_width = satellite_source_tab_width(ui.available_width(), spacing);
        let mut selected_source = None;
        ui.horizontal(|ui| {
            for source in SatelliteSource::ALL {
                if ui
                    .add_sized(
                        egui::vec2(tab_width, PANEL_BUTTON_HEIGHT),
                        egui::Button::selectable(self.satellite_source == source, source.label()),
                    )
                    .on_hover_text(format!("Show {} controls", source.label()))
                    .clicked()
                {
                    selected_source = Some(source);
                }
            }
        });
        if let Some(source) = selected_source {
            self.satellite_source = source;
            self.app_settings.satellite_source = self.satellite_source.slug().to_owned();
            self.mark_app_settings_dirty();
        }

        let mut panel_events = Vec::new();
        let mut load_goes_loop = false;
        let mut load_goes_composite = false;
        let mut load_himawari = false;
        let mut load_himawari_composite = false;
        let mut load_meteosat_latest = false;
        let mut load_meteosat_loop = false;
        let mut load_meteosat_lightning = false;
        let mut save_eumetsat = false;
        let mut test_eumetsat = false;
        let mut forget_eumetsat = false;

        match self.satellite_source {
            SatelliteSource::Goes => {
                panel_kit::subgroup(ui, "GOES live follow", |_ui| {});
                panel_events = self.sat_panel.ui(ui);
                ui.horizontal_wrapped(|ui| {
                    if fixed_action_button(ui, "Load live loop", 110.0)
                        .on_hover_text(
                            "One-shot current-hour ingest for the selected GOES satellite, sector, and layer.",
                        )
                        .clicked()
                    {
                        load_goes_loop = true;
                    }
                    let options = sat_worker::goes_composite_style_options();
                    let selected = options
                        .iter()
                        .find(|(slug, _)| slug == &self.goes_composite_style)
                        .map(|(_, label)| label.as_str())
                        .unwrap_or("GeoColor · C01+C02+C03");
                    egui::ComboBox::from_id_salt("goes_composite_style")
                        .selected_text(selected)
                        .width(210.0)
                        .show_ui(ui, |ui| {
                            for (slug, label) in options {
                                ui.selectable_value(&mut self.goes_composite_style, slug, label);
                            }
                        });
                    if fixed_action_button(ui, "Load RGB", 86.0)
                        .on_hover_text(
                            "Fetch, co-register, and compose the selected GOES RGB product. Natural color products render dark at night.",
                        )
                        .clicked()
                    {
                        load_goes_composite = true;
                    }
                });
            }
            SatelliteSource::Himawari => {
                const IR_BANDS: &[(u8, &str)] = &[
                    (7, "B07 Shortwave IR 3.9"),
                    (8, "B08 Upper WV 6.2"),
                    (9, "B09 Mid WV 6.9"),
                    (10, "B10 Low WV 7.3"),
                    (11, "B11 Cloud-top IR 8.6"),
                    (12, "B12 Ozone 9.6"),
                    (13, "B13 Clean IR 10.4"),
                    (14, "B14 Longwave IR 11.2"),
                    (15, "B15 Dirty IR 12.3"),
                    (16, "B16 CO2 IR 13.3"),
                ];
                const SCOPES: &[(&str, &str)] = &[
                    ("region", "Region · WPac"),
                    ("fulldisk", "Full disk · 4 km"),
                    ("fulldisk2km", "Full disk · 2 km"),
                ];
                let selected_band = IR_BANDS
                    .iter()
                    .find(|(band, _)| *band == self.himawari_band)
                    .map(|(_, label)| *label)
                    .unwrap_or("B13 Clean IR 10.4");
                let selected_scope = SCOPES
                    .iter()
                    .find(|(slug, _)| *slug == self.himawari_true_color_scope)
                    .map(|(_, label)| *label)
                    .unwrap_or("Region · WPac");
                ui.horizontal_wrapped(|ui| {
                    ui.label("IR / WV");
                    egui::ComboBox::from_id_salt("himawari_ir_band")
                        .selected_text(selected_band)
                        .width(180.0)
                        .show_ui(ui, |ui| {
                            for &(band, label) in IR_BANDS {
                                ui.selectable_value(&mut self.himawari_band, band, label);
                            }
                        });
                    if fixed_action_button(ui, "Load latest", 92.0).clicked() {
                        load_himawari = true;
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("True color");
                    let before = self.himawari_true_color_scope.clone();
                    egui::ComboBox::from_id_salt("himawari_true_color_scope")
                        .selected_text(selected_scope)
                        .width(150.0)
                        .show_ui(ui, |ui| {
                            for &(slug, label) in SCOPES {
                                ui.selectable_value(
                                    &mut self.himawari_true_color_scope,
                                    slug.to_owned(),
                                    label,
                                );
                            }
                        });
                    if self.himawari_true_color_scope != before {
                        self.app_settings.himawari_true_color_scope =
                            self.himawari_true_color_scope.clone();
                        self.mark_app_settings_dirty();
                    }
                    if fixed_action_button(ui, "Load RGB", 86.0)
                        .on_hover_text(
                            "Compose AHI true color from real blue, green, and red bands. A focused window overrides the region/full-disk scope.",
                        )
                        .clicked()
                    {
                        load_himawari_composite = true;
                    }
                });
                ui.weak(
                    "Himawari-8/9 has no satellite lightning mapper; JMA LIDEN is a separate ground network.",
                );
            }
            SatelliteSource::Meteosat => {
                let selected =
                    eumetsat::MtgProduct::parse(&self.eumetsat_product).unwrap_or_default();
                ui.horizontal_wrapped(|ui| {
                    ui.label("MTG-I1");
                    egui::ComboBox::from_id_salt("eumetsat_mtg_product")
                        .selected_text(selected.label())
                        .width(205.0)
                        .show_ui(ui, |ui| {
                            for product in eumetsat::MtgProduct::ALL {
                                if ui
                                    .selectable_value(
                                        &mut self.eumetsat_product,
                                        product.slug().to_owned(),
                                        product.label(),
                                    )
                                    .changed()
                                {
                                    self.app_settings.eumetsat_product =
                                        self.eumetsat_product.clone();
                                    self.mark_app_settings_dirty();
                                }
                            }
                        });
                    if fixed_action_button(ui, "Latest", 72.0).clicked() {
                        load_meteosat_latest = true;
                    }
                    if fixed_action_button(ui, "Load loop", 84.0).clicked() {
                        load_meteosat_loop = true;
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Loop frames");
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.eumetsat_loop_frames)
                                .range(2..=36)
                                .speed(1.0),
                        )
                        .changed()
                    {
                        self.app_settings.eumetsat_loop_frames = self.eumetsat_loop_frames;
                        self.mark_app_settings_dirty();
                    }
                    if fixed_action_button(ui, "Lightning · 1 hour", 132.0)
                        .on_hover_text(
                            "Load twelve 5-minute MTG Lightning Imager accumulated-flash-area rasters. This is gridded flash extent, not individual point flashes.",
                        )
                        .clicked()
                    {
                        load_meteosat_lightning = true;
                    }
                    ui.weak("Public EUMETView · no account required");
                });
                if selected == eumetsat::MtgProduct::LightningAfa {
                    ui.hyperlink_to("Open MTG LI color legend", eumetsat::MTG_LI_LEGEND_URL);
                }

                if eumetsat_credentials::DATA_STORE_ACCOUNT_UI_ENABLED {
                    egui::CollapsingHeader::new("Data Store account · optional")
                        .id_salt("eumetsat_account")
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.weak(
                                "The live products above are public. Connect a consumer key and secret for EUMETSAT Data Store access; BowEcho stores them only in this device's credential vault and mints tokens automatically.",
                            );
                            ui.horizontal_wrapped(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.eumetsat_consumer_key)
                                        .hint_text("consumer key")
                                        .password(true)
                                        .desired_width(180.0),
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.eumetsat_consumer_secret)
                                        .hint_text("consumer secret")
                                        .password(true)
                                        .desired_width(190.0),
                                );
                            });
                            ui.horizontal_wrapped(|ui| {
                                let complete = !self.eumetsat_consumer_key.trim().is_empty()
                                    && !self.eumetsat_consumer_secret.trim().is_empty();
                                if ui
                                    .add_enabled(complete, egui::Button::new("Save securely"))
                                    .clicked()
                                {
                                    save_eumetsat = true;
                                }
                                if ui
                                    .add_enabled(complete, egui::Button::new("Test account"))
                                    .clicked()
                                {
                                    test_eumetsat = true;
                                }
                                if ui.button("Forget").clicked() {
                                    forget_eumetsat = true;
                                }
                            });
                            if let Some(status) = &self.eumetsat_account_status {
                                panel_kit::status_block(
                                    ui,
                                    match status {
                                        Ok(message) | Err(message) => message,
                                    },
                                    None,
                                );
                            }
                        });
                }
            }
        }

        panel_kit::subgroup(ui, "Region & resolution", |_ui| {});
        let mut window_changed = false;
        ui.horizontal_wrapped(|ui| {
            window_changed |= ui
                .checkbox(&mut self.sat_window_enabled, "Focused window")
                .on_hover_text(
                    "Use the same saved center and size for provider RGB/MTG requests. Instrument-native GOES/Himawari composites stay at native resolution; EUMETView requests a high-resolution geolocated crop.",
                )
                .changed();
            ui.label("lat");
            let lat = ui.add(
                egui::DragValue::new(&mut self.sat_window_lat_deg)
                    .speed(0.1)
                    .range(-75.0..=75.0)
                    .suffix("°"),
            );
            ui.label("lon");
            let lon = ui.add(
                egui::DragValue::new(&mut self.sat_window_lon_deg)
                    .speed(0.1)
                    .range(-180.0..=180.0)
                    .suffix("°"),
            );
            ui.label("size");
            let size = ui.add(
                egui::DragValue::new(&mut self.sat_window_size_km)
                    .speed(10.0)
                    .range(50..=2000)
                    .suffix(" km"),
            );
            for response in [&lat, &lon, &size] {
                window_changed |=
                    response.drag_stopped() || (response.changed() && !response.dragged());
            }
            if ui.button("Use map center").clicked() {
                self.sat_window_lat_deg = f64::from(self.map_center_lat);
                self.sat_window_lon_deg = f64::from(self.map_center_lon);
                window_changed = true;
            }
        });
        if window_changed {
            self.persist_sat_native_window();
        }

        if load_goes_loop && let Some(sat) = &self.sat {
            self.sat_map_follow = true;
            self.status = "Satellite: loading GOES loop".to_owned();
            self.sat_panel
                .apply_note("GOES loop: queued current-hour ingest".to_owned());
            sat.send(sat_worker::SatRequest::LoadLoop(
                self.sat_panel.spec().clone(),
            ));
        }
        if load_goes_composite && self.sat.is_some() {
            let base = self.sat_panel.spec().clone();
            let style = self.goes_composite_style.clone();
            let window = self.sat_native_window_if_visible(
                sat_window::goes_nominal_sub_lon_deg(&base.satellite),
                &base.satellite,
            );
            self.sat_map_follow = true;
            self.status = format!("Satellite: composing GOES {} {style}", base.sector);
            if let Some(sat) = &self.sat {
                sat.send(sat_worker::SatRequest::IngestLatestGoesComposite(
                    sat_worker::GoesCompositeSpec {
                        satellite: base.satellite,
                        sector: base.sector,
                        style,
                        window,
                        ..sat_worker::GoesCompositeSpec::default()
                    },
                ));
            }
        }
        if load_himawari && let Some(sat) = &self.sat {
            let band = self.himawari_band.clamp(7, 16);
            self.sat_map_follow = true;
            self.status = format!("Satellite: loading latest Himawari-9 B{band:02}");
            sat.send(sat_worker::SatRequest::IngestLatestHimawari(
                sat_worker::HimawariQuickSpec {
                    band,
                    ..sat_worker::HimawariQuickSpec::default()
                },
            ));
        }
        if load_himawari_composite && self.sat.is_some() {
            let window = self
                .sat_native_window_if_visible(sat_window::AHI_NOMINAL_SUB_LON_DEG, "Himawari-9");
            let (full_disk, downsample) = match self.himawari_true_color_scope.as_str() {
                "fulldisk" => (true, 4),
                "fulldisk2km" => (true, 2),
                _ => (
                    false,
                    sat_worker::HimawariCompositeSpec::default().downsample,
                ),
            };
            self.sat_map_follow = true;
            self.status = "Satellite: composing Himawari-9 AHI true color".to_owned();
            if let Some(sat) = &self.sat {
                sat.send(sat_worker::SatRequest::IngestLatestHimawariComposite(
                    sat_worker::HimawariCompositeSpec {
                        window,
                        full_disk,
                        downsample,
                        ..sat_worker::HimawariCompositeSpec::default()
                    },
                ));
            }
        }
        if load_meteosat_latest || load_meteosat_loop || load_meteosat_lightning {
            let product = if load_meteosat_lightning {
                eumetsat::MtgProduct::LightningAfa
            } else {
                eumetsat::MtgProduct::parse(&self.eumetsat_product).unwrap_or_default()
            };
            let frames = if load_meteosat_latest {
                1
            } else if load_meteosat_lightning {
                12
            } else {
                usize::from(self.eumetsat_loop_frames.clamp(2, 36))
            };
            self.queue_meteosat_product(ui.ctx(), product, frames);
        }
        if save_eumetsat || test_eumetsat {
            let credentials = sat_worker::EumetsatAuthSpec {
                consumer_key: self.eumetsat_consumer_key.trim().to_owned(),
                consumer_secret: self.eumetsat_consumer_secret.trim().to_owned(),
            };
            if let Some(sat) = &self.sat {
                if save_eumetsat {
                    sat.send(sat_worker::SatRequest::SaveEumetsatCredentials(
                        credentials.clone(),
                    ));
                }
                if test_eumetsat {
                    self.eumetsat_account_status =
                        Some(Ok("Checking EUMETSAT account…".to_owned()));
                    sat.send(sat_worker::SatRequest::CheckEumetsatAccount(credentials));
                }
            }
        }
        if forget_eumetsat {
            self.eumetsat_consumer_key.clear();
            self.eumetsat_consumer_secret.clear();
            self.eumetsat_account_status = None;
            if let Some(sat) = &self.sat {
                sat.send(sat_worker::SatRequest::ForgetEumetsatCredentials);
            }
        }
        panel_events
    }

    /// Satellite body (follow config + frame player), window and pane
    /// alike. Returned events feed `handle_satellite_events`.
    pub(crate) fn satellite_pane_body(
        &mut self,
        ui: &mut egui::Ui,
    ) -> (Vec<rw_ui::SatelliteEvent>, Vec<rw_ui::SatPlayerEvent>) {
        // The provider controls are intentionally information-dense. In a
        // short floating window or a shallow dock they can consume the
        // parent's whole clip rect before rw-ui's player measures its image
        // area. The player then clamps its native-frame scale to 0.01, which
        // turns a healthy 5000x3000 ABI frame into a misleading 50x30 dark
        // thumbnail beside the "texture cache" footer. Give the pane a real
        // scrolling content surface and reserve a finite player rectangle;
        // the controls remain reachable while the preview never collapses.
        let mut events = (Vec::new(), Vec::new());
        egui::ScrollArea::vertical()
            .id_salt("bowecho-satellite-pane-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                events = self.satellite_pane_contents(ui);
            });
        events
    }

    fn satellite_pane_contents(
        &mut self,
        ui: &mut egui::Ui,
    ) -> (Vec<rw_ui::SatelliteEvent>, Vec<rw_ui::SatPlayerEvent>) {
        let panel_events = self.satellite_provider_controls(ui);
        if self.satellite_source != SatelliteSource::Goes
            && let Some(activity) = satellite_activity_status(&self.status)
        {
            panel_kit::subgroup(ui, "Live progress", |_ui| {});
            panel_kit::status_block(ui, activity, None);
        }
        panel_kit::subgroup(ui, "Display & output", |_ui| {});
        if self.sat_last_frame.is_some() || self.sat_layer.is_some() {
            let mut map_request: Option<(rw_ui::SatRunKey, u16)> = None;
            let mut plot_request: Option<(rw_ui::SatRunKey, u16)> = None;
            let mut center_on_satellite = false;
            let frame_available = self.sat_last_frame.is_some();
            let center_action_available = self
                .sat_layer
                .as_ref()
                .zip(self.sat_layer_texture.as_ref())
                .is_some_and(|(layer, (_, generation, _, has_visible_pixels))| {
                    layer.generation == *generation
                        && !*has_visible_pixels
                        && layer
                            .lut
                            .lookup(self.map_center_lat, self.map_center_lon)
                            .is_none()
                });
            let button_label = if self.sat_layer.is_some() {
                "Refresh map frame"
            } else {
                "Show on radar map"
            };
            ui.horizontal_wrapped(|ui| {
                let follow_was_enabled = self.sat_map_follow;
                if ui
                    .add_enabled(
                        frame_available,
                        egui::Checkbox::new(&mut self.sat_map_follow, "Map follows player"),
                    )
                    .on_hover_text(
                        "When enabled, satellite play/scrub changes update the radar-map satellite layer too.",
                    )
                    .changed()
                    && self.sat_map_follow
                    && !follow_was_enabled
                {
                    map_request = self.sat_last_frame.clone();
                }
                if ui
                    .add_enabled(frame_available, egui::Button::new(button_label))
                    .on_hover_text(SATELLITE_MAP_LAYER_HOVER)
                    .clicked()
                {
                    self.sat_map_follow = true;
                    map_request = self.sat_last_frame.clone();
                }
                if ui
                    .add_enabled(frame_available, egui::Button::new("Native plot"))
                    .on_hover_text(
                        "Render the selected satellite frame through BowEcho's georeferenced \
                         native plotter. Scalar bands retain raw values/units; RGB composites \
                         retain their baked colors.",
                    )
                    .clicked()
                {
                    plot_request = self.sat_last_frame.clone();
                }
                if center_action_available
                    && ui
                    .button("Center on satellite coverage")
                    .on_hover_text(
                        "Move the radar map to a visible, geolocated pixel in the current satellite frame.",
                    )
                    .clicked()
                {
                    center_on_satellite = true;
                }
                if let Some(layer) = &mut self.sat_layer {
                    ui.checkbox(&mut layer.visible, "show");
                    ui.weak(format!("{} {:04}Z", layer.key, layer.hhmm));
                }
            });
            if let Some((key, hhmm)) = map_request {
                self.request_sat_map_frame(key, hhmm);
            }
            if let Some((key, hhmm)) = plot_request
                && let Some(sat) = &self.sat
            {
                self.sat_panel
                    .apply_note(format!("Loading native plot for {key} {hhmm:04}Z"));
                sat.send(sat_worker::SatRequest::LoadFrameForPlot { key, hhmm });
            }
            if center_on_satellite {
                let target = self.sat_layer.as_ref().and_then(|layer| {
                    satellite_visible_coverage_point(
                        layer.image.as_ref(),
                        layer.grid.as_ref(),
                        layer.flip_rows,
                    )
                });
                if let Some((lat, lon)) = target {
                    self.center_map_on(lat, lon);
                    self.status = format!("Satellite map centered on {lat:.2}°, {lon:.2}°");
                    ui.ctx().request_repaint();
                } else {
                    self.status =
                        "Satellite frame has no visible geolocated pixel to center on".to_owned();
                }
            }
        }
        let mut selected_enhancement = self.sat_ir_enhancement;
        ui.horizontal_wrapped(|ui| {
            ui.label("IR enhancement");
            egui::ComboBox::from_id_salt("sat_ir_enhancement")
                .selected_text(selected_enhancement.label())
                .width(170.0)
                .show_ui(ui, |ui| {
                    for option in sat_worker::IrEnhancement::ALL {
                        ui.selectable_value(&mut selected_enhancement, option, option.label());
                    }
                })
                .response
                .on_hover_text(
                    "Absolute-temperature color curve for IR brightness-temperature bands \
                     (GOES ABI and Himawari AHI 7-16): recommended CIMSS Style, Natural NOAA \
                     heritage grayscale, the Dvorak BD curve, AVN, Funktop, rainbow, or plain \
                     grayscale.",
                );
        });
        // Without this hint the picker looks silently dead on a store full
        // of pre-calibration frames (they render via the legacy auto-stretch
        // no matter which enhancement is selected).
        if self
            .sat_last_frame
            .as_ref()
            .is_some_and(|frame| self.sat_legacy_frames.contains(frame))
        {
            ui.weak(
                "This stored frame predates the true-Kelvin calibration and uses the legacy \
                 auto-stretch — IR enhancements apply to newly ingested frames.",
            );
        }
        if selected_enhancement != self.sat_ir_enhancement {
            self.sat_ir_enhancement = selected_enhancement;
            self.app_settings.sat_ir_enhancement = selected_enhancement.slug().to_string();
            self.mark_app_settings_dirty();
            if let Some(sat) = &self.sat {
                sat.send(sat_worker::SatRequest::SetIrEnhancement(
                    selected_enhancement,
                ));
                // Recolor EVERYTHING the player can show, not just the
                // playhead: the player caches colored textures per scan
                // time, so a running loop would keep mixing old and new
                // palettes. Each re-pushed frame replaces its cached
                // texture in place (playback state untouched); the map
                // frame refreshes below.
                for (key, hhmm) in sat_enhancement_refresh_frames(
                    &self.sat_run_listings,
                    self.sat_player.selected_run(),
                    self.sat_last_frame.as_ref(),
                ) {
                    sat.send(sat_worker::SatRequest::LoadFrame { key, hhmm });
                }
            }
            // The map layer recolors by ITS OWN identity (layer, else the
            // in-flight/queued request): timeline follow can point the map
            // at a different scan than the player, and keying this off
            // `sat_last_frame` left the map on the old palette then.
            if let Some((key, hhmm)) = self.sat_map_recolor_target() {
                self.request_sat_map_frame(key, hhmm);
            }
        }
        panel_kit::subgroup(ui, "Saved loop", |_ui| {});
        let player_height = satellite_player_panel_height(ui.available_width());
        let player_events = ui
            .allocate_ui_with_layout(
                egui::vec2(ui.available_width(), player_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| self.sat_player.ui(ui),
            )
            .inner;
        (panel_events, player_events)
    }

    pub(crate) fn clear_satellite_display_for_spec_change(&mut self) {
        self.sat_last_frame = None;
        self.sat_legacy_frames.clear();
        self.sat_run_listings.clear();
        self.sat_player.set_runs(Vec::new());
        self.sat_layer = None;
        self.sat_layer_texture = None;
        self.sat_layer_build_rx = None;
        self.sat_layer_render_rx = None;
        self.sat_map_inflight = None;
        self.sat_map_pending = None;
        self.sat_layer_generation = self.sat_layer_generation.wrapping_add(1);
        // `sat_lut_cache` deliberately survives: entries are keyed by grid
        // CONTENT (sha256), so a spec change can never make them wrong —
        // and toggling back to a spec whose grid was already indexed
        // reinstalls the layer without the multi-second LUT rebuild.
    }

    pub(crate) fn satellite_run_key_matches_current_spec(&self, key: &rw_ui::SatRunKey) -> bool {
        sat_worker::run_filters_for_spec(self.sat_panel.spec())
            .map(|(model, prefixes)| {
                satellite_run_key_matches_resolved_spec(key, &model, &prefixes)
            })
            .unwrap_or(true)
    }

    pub(crate) fn satellite_runs_for_current_spec(
        &self,
        runs: Vec<rw_ui::SatRunListing>,
    ) -> Vec<rw_ui::SatRunListing> {
        let Ok((model, prefixes)) = sat_worker::run_filters_for_spec(self.sat_panel.spec()) else {
            return runs;
        };
        runs.into_iter()
            .filter(|run| satellite_run_key_matches_resolved_spec(&run.key, &model, &prefixes))
            .collect()
    }

    pub(crate) fn request_sat_map_frame(&mut self, key: rw_ui::SatRunKey, hhmm: u16) {
        if !self.satellite_run_key_matches_current_spec(&key) {
            return;
        }
        // Deliberately NOT mirrored into `sat_last_frame`: that field is
        // the PLAYER's displayed frame (FrameSelected / SelectFrame /
        // loaded-frame events). Writing the map target here let a timeline
        // sync overwrite the player identity, which then re-anchored the
        // timeline search onto the map's own run — the self-reinforcing pin
        // that held a weeks-old frame on the map.
        if self.sat_layer_build_rx.is_some() || self.sat_map_inflight.is_some() {
            self.sat_map_pending = Some((key, hhmm));
            return;
        }
        let Some(sat) = &self.sat else {
            self.sat_map_pending = Some((key, hhmm));
            return;
        };
        sat.send(sat_worker::SatRequest::LoadFrameForMap {
            key: key.clone(),
            hhmm,
        });
        self.sat_map_inflight = Some((key, hhmm));
    }

    fn flush_pending_sat_map_request(&mut self) {
        if self.sat_layer_build_rx.is_some() || self.sat_map_inflight.is_some() {
            return;
        }
        if let Some((key, hhmm)) = self.sat_map_pending.take() {
            self.request_sat_map_frame(key, hhmm);
        }
    }

    /// The (key, hhmm) the radar-map satellite layer currently shows — or
    /// is already fetching/queued to show. This is the identity an
    /// IR-enhancement change must re-request: the map tracks its own frame
    /// (timeline follow can point it at a different scan than the player),
    /// so recoloring by `sat_last_frame` left the map on the old palette
    /// whenever the two diverged.
    pub(crate) fn sat_map_recolor_target(&self) -> Option<(rw_ui::SatRunKey, u16)> {
        self.sat_layer
            .as_ref()
            .map(|layer| (layer.key.clone(), layer.hhmm))
            .or_else(|| self.sat_map_inflight.clone())
            .or_else(|| self.sat_map_pending.clone())
    }

    /// A worker map frame landed. Install it only while its request is
    /// still remembered: removing the satellite layer (layers rail)
    /// clears `sat_map_inflight`, so a response already in flight at
    /// remove time must not resurrect the layer. A non-matching response
    /// (superseded request) is dropped without touching the latch.
    pub(crate) fn apply_sat_map_frame_response(
        &mut self,
        frame: sat_worker::SatMapFrame,
        ctx: &egui::Context,
    ) {
        if !sat_map_request_matches(&self.sat_map_inflight, &frame.key, frame.hhmm) {
            return;
        }
        self.sat_map_inflight = None;
        if self.satellite_run_key_matches_current_spec(&frame.key) {
            self.install_sat_layer(frame, ctx);
        }
    }

    pub(crate) fn handle_satellite_events(
        &mut self,
        panel_events: Vec<rw_ui::SatelliteEvent>,
        player_events: Vec<rw_ui::SatPlayerEvent>,
    ) {
        for event in panel_events {
            match event {
                rw_ui::SatelliteEvent::SpecChanged(spec) => {
                    self.clear_satellite_display_for_spec_change();
                    if let Some(sat) = &self.sat {
                        sat.send(sat_worker::SatRequest::Validate(spec));
                        sat.send(sat_worker::SatRequest::Scan);
                    }
                }
                rw_ui::SatelliteEvent::StartRequested(spec) => {
                    if let Some(sat) = &self.sat {
                        sat.send(sat_worker::SatRequest::Follow(spec));
                    }
                }
                rw_ui::SatelliteEvent::StopRequested => {
                    if let Some(sat) = &self.sat {
                        sat.stop_follow();
                    }
                }
            }
        }
        for event in player_events {
            match event {
                rw_ui::SatPlayerEvent::FrameWanted { key, hhmm } => {
                    if let Some(sat) = &self.sat {
                        sat.send(sat_worker::SatRequest::LoadFrame { key, hhmm });
                    }
                }
                rw_ui::SatPlayerEvent::FrameSelected { key, hhmm } => {
                    self.sat_last_frame = Some((key.clone(), hhmm));
                    if self.sat_map_follow {
                        self.request_sat_map_frame(key, hhmm);
                    }
                }
                rw_ui::SatPlayerEvent::RefreshRequested => {
                    if let Some(sat) = &self.sat {
                        sat.send(sat_worker::SatRequest::Scan);
                    }
                }
            }
        }
    }

    pub(crate) fn pump_sat_responses(
        &mut self,
        ctx: &egui::Context,
    ) -> Option<sat_plot::SatellitePlotSource> {
        let mut plot_source = None;
        // Transient borrow per message so handlers can take &mut self.
        while let Some(response) = self.sat.as_ref().and_then(|sat| sat.try_recv()) {
            match response {
                sat_worker::SatResponse::SpecStatus(status) => {
                    self.sat_panel.set_spec_status(status)
                }
                sat_worker::SatResponse::EumetsatAccount(result) => {
                    let note = match &result {
                        Ok(message) => message.clone(),
                        Err(message) => message.clone(),
                    };
                    self.eumetsat_account_status = Some(result);
                    self.sat_panel.apply_note(note);
                }
                sat_worker::SatResponse::EumetsatCredentialsLoaded(result) => match result {
                    Ok(Some(credentials)) => {
                        self.eumetsat_consumer_key = credentials.consumer_key;
                        self.eumetsat_consumer_secret = credentials.consumer_secret;
                        self.eumetsat_account_status = Some(Ok(
                            "Saved EUMETSAT account loaded from this device".to_owned(),
                        ));
                    }
                    Ok(None) => {}
                    Err(message) => {
                        self.eumetsat_account_status = Some(Err(message));
                    }
                },
                sat_worker::SatResponse::EumetsatCredentialsSaved(result) => {
                    let note = match &result {
                        Ok(message) => message.clone(),
                        Err(message) => message.clone(),
                    };
                    self.eumetsat_account_status = Some(result);
                    self.sat_panel.apply_note(note);
                }
                sat_worker::SatResponse::Runs(runs) => {
                    let runs = self.satellite_runs_for_current_spec(runs);
                    self.sat_run_listings = runs.clone();
                    self.sat_player.set_runs(runs);
                }
                sat_worker::SatResponse::FollowStarted => self.sat_panel.begin_follow(),
                sat_worker::SatResponse::FollowFinished(result) => {
                    if self.sat_panel.is_running() {
                        self.sat_panel.finish_follow(result);
                    } else if let Err(message) = result {
                        self.sat_panel.set_spec_status(Err(message));
                    }
                }
                sat_worker::SatResponse::PollDone { band, new_keys, ms } => {
                    self.sat_panel.apply_poll_done(band, new_keys, ms);
                }
                sat_worker::SatResponse::DownloadStarted { id, label, bytes } => {
                    self.status = format!(
                        "Satellite: {label} · downloading {}",
                        rw_ui::format_bytes(bytes)
                    );
                    self.sat_panel.apply_download_started(id, label, bytes);
                }
                sat_worker::SatResponse::DownloadDone { id, ms, cache_hit } => {
                    self.status = format!(
                        "Satellite: download complete in {ms} ms{} · decoding / storing",
                        if cache_hit { " · cache hit" } else { "" }
                    );
                    self.sat_panel.apply_download_done(&id, ms, cache_hit);
                }
                sat_worker::SatResponse::FrameWritten {
                    id,
                    model,
                    run,
                    hhmm,
                    bytes,
                    encode_ms,
                    select_live_run,
                } => {
                    self.status = format!(
                        "Satellite: stored {run}/t{hhmm:04} · {} in {encode_ms} ms",
                        rw_ui::format_bytes(bytes)
                    );
                    self.sat_panel
                        .apply_frame_written(&id, run.clone(), hhmm, bytes, encode_ms);
                    if let Some(sat) = &self.sat {
                        sat.send(satellite_frame_written_refresh_request(
                            model,
                            run,
                            hhmm,
                            select_live_run,
                        ));
                    }
                }
                sat_worker::SatResponse::Evicted { frames, bytes } => {
                    self.sat_panel.apply_evicted(frames, bytes);
                    if let Some(sat) = &self.sat {
                        sat.send(sat_worker::SatRequest::Scan);
                    }
                }
                sat_worker::SatResponse::Sleeping { ms } => self.sat_panel.apply_sleeping(ms),
                sat_worker::SatResponse::Note(message) => {
                    self.status = format!("Satellite: {message}");
                    self.sat_panel.apply_note(message);
                }
                sat_worker::SatResponse::DiskUsage(usage) => self.sat_panel.set_disk_usage(usage),
                sat_worker::SatResponse::SelectFrame { key, hhmm } => {
                    if !self.satellite_run_key_matches_current_spec(&key) {
                        continue;
                    }
                    self.sat_last_frame = Some((key.clone(), hhmm));
                    self.sat_player.select_frame(key.clone(), hhmm);
                    if let Some(sat) = &self.sat {
                        sat.send(sat_worker::SatRequest::LoadFrame {
                            key: key.clone(),
                            hhmm,
                        });
                    }
                    if self.sat_map_follow
                        && !self.satellite_map_frame_current_or_scheduled(&key, hhmm)
                    {
                        self.request_sat_map_frame(key, hhmm);
                    }
                }
                sat_worker::SatResponse::MapFrame(result) => match *result {
                    Ok(frame) => {
                        self.apply_sat_map_frame_response(frame, ctx);
                        self.flush_pending_sat_map_request();
                    }
                    Err(message) => {
                        self.sat_map_inflight = None;
                        self.status = format!("Sat layer: {message}");
                        self.flush_pending_sat_map_request();
                    }
                },
                sat_worker::SatResponse::PlotFrame { key, hhmm, result } => match *result {
                    Ok(source) => {
                        self.sat_panel
                            .apply_note(format!("Native plot ready for {key} {hhmm:04}Z"));
                        plot_source = Some(source);
                    }
                    Err(message) => {
                        self.sat_panel.apply_note(format!("native plot: {message}"));
                    }
                },
                sat_worker::SatResponse::Frame {
                    key,
                    hhmm,
                    legacy,
                    result,
                } => match *result {
                    Ok(frame) => {
                        if !self.satellite_run_key_matches_current_spec(&key) {
                            continue;
                        }
                        if legacy {
                            self.sat_legacy_frames.insert((key.clone(), hhmm));
                        } else {
                            self.sat_legacy_frames.remove(&(key.clone(), hhmm));
                        }
                        self.sat_last_frame = Some((key.clone(), hhmm));
                        let (frame, resized) = sat_player_frame_within_texture_limit(frame);
                        if let Some((old_size, new_size)) = resized {
                            self.sat_panel.apply_note(format!(
                                "preview downsampled {}x{} -> {}x{} for GPU texture limit",
                                old_size[0], old_size[1], new_size[0], new_size[1]
                            ));
                        }
                        self.sat_player.set_frame(frame);
                        if self.sat_map_follow
                            && !self.satellite_map_frame_current_or_scheduled(&key, hhmm)
                        {
                            self.request_sat_map_frame(key, hhmm);
                        }
                    }
                    Err(message) => {
                        if self.sat_player.selected_run() == Some(&key) {
                            self.sat_player.frame_failed(hhmm);
                        }
                        self.sat_panel.apply_note(format!("frame load: {message}"));
                    }
                },
            }
        }
        plot_source
    }

    /// Route a raw satellite plot payload into the existing Model native-plot
    /// surface without changing the model run/field selection. The Model
    /// viewer is initialized lazily on profiles whose model store is empty.
    pub(crate) fn open_satellite_native_plot(
        &mut self,
        ctx: &egui::Context,
        source: sat_plot::SatellitePlotSource,
    ) {
        self.model_enabled = true;
        if self.model_dock.is_none() {
            self.model_dock = Some(self.new_model_data_dock(ctx, settings::model_store_dir()));
        }
        if let Some(model_dock) = self.model_dock.as_mut() {
            model_dock.open_satellite_plot(source);
        }
        self.open_viewer(dock::WorkspacePane::Model);
    }

    /// Install a GOES frame as the sat map layer (LUT built on a
    /// background thread; same machinery as the model layer).
    /// Cached LUT for a grid identity, refreshing its LRU position. Empty
    /// hashes (synthesized grids) never match — they carry no identity.
    pub(crate) fn sat_lut_cache_get(
        &mut self,
        grid_hash: &str,
        nx: usize,
        ny: usize,
    ) -> Option<Arc<model_layer::InverseLut>> {
        if grid_hash.is_empty() {
            return None;
        }
        let position = self
            .sat_lut_cache
            .iter()
            .position(|entry| entry.grid_hash == grid_hash && entry.nx == nx && entry.ny == ny)?;
        let entry = self.sat_lut_cache.remove(position);
        let lut = Arc::clone(&entry.lut);
        self.sat_lut_cache.insert(0, entry);
        Some(lut)
    }

    pub(crate) fn sat_lut_cache_insert(
        &mut self,
        grid_hash: String,
        nx: usize,
        ny: usize,
        lut: Arc<model_layer::InverseLut>,
    ) {
        if grid_hash.is_empty() {
            return;
        }
        self.sat_lut_cache
            .retain(|entry| entry.grid_hash != grid_hash || entry.nx != nx || entry.ny != ny);
        self.sat_lut_cache.insert(
            0,
            SatLutCacheEntry {
                grid_hash,
                nx,
                ny,
                lut,
            },
        );
        self.sat_lut_cache.truncate(SAT_LUT_CACHE_CAP);
    }

    fn install_sat_layer(&mut self, frame: sat_worker::SatMapFrame, ctx: &egui::Context) {
        if self.sat_layer_build_rx.is_some() {
            self.sat_map_pending = Some((frame.key, frame.hhmm));
            return;
        }
        let key = frame.key.clone();
        let hhmm = frame.hhmm;
        let nx = frame.grid.nx;
        let ny = frame.grid.ny;
        if let Some(existing) = &self.sat_layer
            && existing.key == key
            && existing.nx == nx
            && existing.ny == ny
            && existing.flip_rows == frame.flip_rows
        {
            let generation = self.sat_layer_generation + 1;
            self.sat_layer_generation = generation;
            self.sat_layer = Some(SatMapLayer {
                key,
                hhmm,
                image: Arc::new(frame.image),
                grid: Arc::clone(&existing.grid),
                lut: Arc::clone(&existing.lut),
                nx,
                ny,
                flip_rows: existing.flip_rows,
                opacity: existing.opacity,
                visible: existing.visible,
                generation,
            });
            // Keep the previous map texture visible until the next frame's
            // viewport render is ready. Clearing here makes playback blink
            // blank between frames, especially now that sat overlays render
            // at full viewport resolution.
            self.sat_layer_render_rx = None;
            self.status = format!("Satellite map: {hhmm:04}Z");
            return;
        }
        // Different run key, but a grid we have already indexed (successive
        // runs of a product write bit-identical grids; spec toggles come
        // back to the same grid): reuse the cached LUT synchronously
        // instead of parking playback behind a multi-second rebuild.
        if let Some(lut) = self.sat_lut_cache_get(&frame.grid.hash, nx, ny) {
            let generation = self.sat_layer_generation + 1;
            self.sat_layer_generation = generation;
            let opacity = self
                .sat_layer
                .as_ref()
                .map(|layer| layer.opacity)
                .unwrap_or_else(|| self.style_registry.drapes().goes_opacity);
            let visible = self
                .sat_layer
                .as_ref()
                .map(|layer| layer.visible)
                .unwrap_or(true);
            self.sat_layer = Some(SatMapLayer {
                key,
                hhmm,
                image: Arc::new(frame.image),
                grid: frame.grid,
                lut,
                nx,
                ny,
                flip_rows: frame.flip_rows,
                opacity,
                visible,
                generation,
            });
            // As above: the previous texture stays up as a placeholder until
            // this generation's viewport render lands.
            self.sat_layer_render_rx = None;
            self.status = format!("Satellite map: {hhmm:04}Z");
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.sat_layer_build_rx = Some(receiver);
        let generation = self.sat_layer_generation + 1;
        let initial_opacity = self
            .sat_layer
            .as_ref()
            .map(|layer| layer.opacity)
            .unwrap_or_else(|| self.style_registry.drapes().goes_opacity);
        let initial_visible = self
            .sat_layer
            .as_ref()
            .map(|layer| layer.visible)
            .unwrap_or(true);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let layer =
                model_layer::InverseLut::build_with_shape(&frame.grid.lat, &frame.grid.lon, nx, ny)
                    .map(|lut| SatMapLayer {
                        key: frame.key,
                        hhmm: frame.hhmm,
                        image: Arc::new(frame.image),
                        grid: Arc::clone(&frame.grid),
                        lut: Arc::new(lut),
                        nx,
                        ny,
                        flip_rows: frame.flip_rows,
                        opacity: initial_opacity,
                        visible: initial_visible,
                        generation,
                    });
            let _ = sender.send(layer);
            ctx.request_repaint();
        });
        self.status = format!("Building satellite layer {key} {hhmm:04}Z");
    }

    pub(crate) fn poll_sat_layer(&mut self, ctx: &egui::Context) {
        if let Some(receiver) = &self.sat_layer_build_rx {
            match receiver.try_recv() {
                Ok(layer) => {
                    self.sat_layer_build_rx = None;
                    if let Some(layer) = layer {
                        // Remember the freshly built LUT under the grid's
                        // content hash: every later frame of this run — and
                        // of successive runs writing the identical grid —
                        // installs without another rebuild.
                        self.sat_lut_cache_insert(
                            layer.grid.hash.clone(),
                            layer.nx,
                            layer.ny,
                            Arc::clone(&layer.lut),
                        );
                        self.sat_layer_generation = layer.generation;
                        self.sat_layer = Some(layer);
                        // Keep the old texture as a placeholder while the
                        // new layer renders; draw_sat_layer detects the
                        // generation mismatch and refreshes it.
                        self.status = "Satellite layer active".to_owned();
                    } else {
                        self.status = "Satellite grid has no geolocation".to_owned();
                    }
                    ctx.request_repaint();
                    self.flush_pending_sat_map_request();
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    // A dead build thread must release the latch, or every
                    // later map request parks in `sat_map_pending` forever.
                    self.sat_layer_build_rx = None;
                    self.flush_pending_sat_map_request();
                }
            }
        }
        if let Some(receiver) = &self.sat_layer_render_rx {
            match receiver.try_recv() {
                Ok((generation, key, image, _ms)) => {
                    self.sat_layer_render_rx = None;
                    if let Some(layer) = self
                        .sat_layer
                        .as_ref()
                        .filter(|layer| layer.generation == generation)
                    {
                        let has_visible_pixels = satellite_render_has_visible_pixels(&image);
                        let outside_coverage =
                            layer.lut.lookup(key.center_lat, key.center_lon).is_none();
                        let texture =
                            ctx.load_texture("sat-layer", image, egui::TextureOptions::LINEAR);
                        self.sat_layer_texture =
                            Some((texture, generation, key, has_visible_pixels));
                        self.status = if has_visible_pixels {
                            format!("Satellite map: {:04}Z", layer.hhmm)
                        } else if outside_coverage {
                            "Satellite map: current view is outside this sector — use Center on satellite coverage".to_owned()
                        } else {
                            "Satellite map: frame is transparent in this view — try an IR layer at night".to_owned()
                        };
                    }
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.sat_layer_render_rx = None,
            }
        }
    }

    /// Draw the satellite layer (world-anchored; renders at viewport
    /// resolution on a background thread, exactly like the model layer).
    pub(crate) fn draw_sat_layer(&mut self, painter: &egui::Painter, rect: egui::Rect) {
        let Some(layer) = &self.sat_layer else {
            return;
        };
        if !layer.visible {
            return;
        }
        let view = self.model_layer_current_view();
        let current = self
            .sat_layer_texture
            .as_ref()
            .filter(|(_, generation, _, _)| *generation == layer.generation);
        let needs_render = current
            .map(|(_, _, have, _)| model_layer_view_needs_rerender(have, &view))
            .unwrap_or(true);
        let defer_render = map_layer_rerender_deferred(painter.ctx());
        if needs_render && !defer_render && self.sat_layer_render_rx.is_none() {
            let (sender, receiver) = mpsc::channel();
            self.sat_layer_render_rx = Some(receiver);
            let generation = layer.generation;
            let image_src = Arc::clone(&layer.image);
            let grid = Arc::clone(&layer.grid);
            let lut = Arc::clone(&layer.lut);
            let (nx, ny, flip) = (layer.nx, layer.ny, layer.flip_rows);
            let render_view = view;
            let center_lat = view.center_lat as f64;
            let center_lon = view.center_lon as f64;
            let km_per_pt = 111.32 / view.map_scale as f64;
            let (w_pts, h_pts) = (rect.width() as f64, rect.height() as f64);
            let ctx = painter.ctx().clone();
            thread::spawn(move || {
                let render_start = Instant::now();
                let w = w_pts.max(8.0) as usize;
                let h = h_pts.max(8.0) as usize;
                let mut pixels = vec![egui::Color32::TRANSPARENT; w * h];
                let sample_ctx = SatMapSampleCtx {
                    image: image_src.as_ref(),
                    grid: grid.as_ref(),
                    nx,
                    ny,
                    flip_rows: flip,
                };
                for (i, px) in pixels.iter_mut().enumerate() {
                    let x = (i % w) as f64;
                    let y = (i / w) as f64;
                    let east_km = (x - w as f64 / 2.0) * km_per_pt;
                    let north_km = (h as f64 / 2.0 - y) * km_per_pt;
                    let (lat, lon) = aeqd_inverse_km(center_lat, center_lon, east_km, north_km);
                    let Some(index) = lut.lookup(lat as f32, lon as f32) else {
                        continue;
                    };
                    let color = sample_sat_map_color(&sample_ctx, index, lat as f32, lon as f32);
                    if color.a() > 0 {
                        *px = color;
                    }
                }
                let image = egui::ColorImage {
                    size: [w, h],
                    source_size: egui::vec2(w as f32, h as f32),
                    pixels,
                };
                let _ = sender.send((
                    generation,
                    render_view,
                    image,
                    render_start.elapsed().as_secs_f32() * 1000.0,
                ));
                ctx.request_repaint();
            });
        }
        if let Some((texture, _, rendered, has_visible_pixels)) = &self.sat_layer_texture {
            let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
            let tint = egui::Color32::from_white_alpha((layer.opacity * 255.0) as u8);
            if model_layer_view_needs_rerender(rendered, &view) {
                // Mid-gesture (owner report: "every time we zoom in the sat
                // image disappears until we stop"): keep painting the last
                // rendered raster, world-anchored into the moving view like
                // the radar texture — momentary AEQD distortion is expected
                // and the fresh render above swaps in when it lands.
                let anchored =
                    anchored_sat_texture_rect(rect, texture.size_vec2(), rendered, &view);
                if anchored.is_finite() && anchored.intersects(rect) {
                    painter.image(texture.id(), anchored, uv, tint);
                }
            } else {
                painter.image(texture.id(), rect, uv, tint);
            }
            if !has_visible_pixels && !model_layer_view_needs_rerender(rendered, &view) {
                draw_satellite_no_visible_notice(
                    painter,
                    rect,
                    layer.lut.lookup(view.center_lat, view.center_lon).is_none(),
                );
            }
        }
        if layer.key.model == "mtg_i1" {
            let year = layer
                .key
                .run
                .split('_')
                .find(|token| token.len() == 8 && token.bytes().all(|byte| byte.is_ascii_digit()))
                .and_then(|token| token.get(..4))
                .unwrap_or("2026");
            let text = format!("Contains modified EUMETSAT Meteosat data {year}.");
            let style = painter.ctx().global_style();
            let visuals = &style.visuals;
            let galley = painter.layout_no_wrap(
                text,
                egui::FontId::proportional(10.0),
                visuals.text_color(),
            );
            let size = galley.size() + egui::vec2(12.0, 8.0);
            let card = egui::Rect::from_min_size(
                egui::pos2(rect.right() - size.x - 8.0, rect.top() + 62.0),
                size,
            );
            painter.rect_filled(card, 4.0, visuals.window_fill());
            painter.rect_stroke(card, 4.0, visuals.window_stroke(), egui::StrokeKind::Inside);
            painter.galley(
                card.min + egui::vec2(6.0, 4.0),
                galley,
                visuals.text_color(),
            );
        }
    }
}

/// Player panel height reserved inside the satellite pane's scroll surface.
/// About 112 points belong to transport/run chrome; the remainder is a
/// practical preview that grows with pane width without making wide panes
/// absurdly tall.
fn satellite_player_panel_height(available_width: f32) -> f32 {
    112.0 + (available_width * 0.40).clamp(240.0, 420.0)
}

fn satellite_render_has_visible_pixels(image: &egui::ColorImage) -> bool {
    image.pixels.iter().any(|pixel| pixel.a() > 0)
}

/// Pick an actual visible, geolocated source pixel near the frame center for
/// the explicit "Center on satellite coverage" action. Sampling keeps the
/// common multi-million-pixel path cheap; a full fallback handles tiny or
/// unusually sparse windows without inventing a bbox center (which is wrong
/// for antimeridian-crossing grids).
fn satellite_visible_coverage_point(
    image: &egui::ColorImage,
    grid: &rw_store::grid::GridFile,
    flip_rows: bool,
) -> Option<(f32, f32)> {
    let (nx, ny) = (grid.nx, grid.ny);
    let len = nx.checked_mul(ny)?;
    if nx == 0
        || ny == 0
        || grid.lat.len() != len
        || grid.lon.len() != len
        || image.size != [nx, ny]
        || image.pixels.len() != len
    {
        return None;
    }
    let center_row = ny / 2;
    let center_col = nx / 2;
    let candidate = |index: usize| {
        let (lat, lon) = (*grid.lat.get(index)?, *grid.lon.get(index)?);
        if !lat.is_finite()
            || !lon.is_finite()
            || sat_map_grid_color(image, index, nx, ny, flip_rows)?.a() == 0
        {
            return None;
        }
        let row = index / nx;
        let col = index % nx;
        let distance =
            (row.abs_diff(center_row) as u64).pow(2) + (col.abs_diff(center_col) as u64).pow(2);
        Some((distance, lat, lon))
    };
    let center_index = center_row * nx + center_col;
    if let Some((_, lat, lon)) = candidate(center_index) {
        return Some((lat, lon));
    }

    let stride = (len / 65_536).max(1);
    let nearest = |stride: usize| {
        (0..len)
            .step_by(stride)
            .filter_map(&candidate)
            .min_by_key(|(distance, _, _)| *distance)
            .map(|(_, lat, lon)| (lat, lon))
    };
    nearest(stride).or_else(|| (stride > 1).then(|| nearest(1)).flatten())
}

fn satellite_frame_written_refresh_request(
    model: String,
    run: String,
    hhmm: u16,
    select_live_run: bool,
) -> sat_worker::SatRequest {
    if select_live_run && matches!(model.as_str(), "g16" | "g17" | "g18" | "g19") {
        sat_worker::SatRequest::ScanAndSelect {
            key: rw_ui::SatRunKey { model, run },
            hhmm,
        }
    } else {
        sat_worker::SatRequest::Scan
    }
}

fn draw_satellite_no_visible_notice(
    painter: &egui::Painter,
    rect: egui::Rect,
    outside_coverage: bool,
) {
    let text = if outside_coverage {
        "No visible satellite pixels in this map view\nCurrent map is outside this satellite sector\nUse Center on satellite coverage in the Satellite panel"
    } else {
        "No visible satellite pixels in this map view\nThis frame is transparent here; try an IR layer at night"
    };
    let style = painter.ctx().global_style();
    let visuals = &style.visuals;
    let galley = painter.layout(
        text.to_owned(),
        egui::FontId::proportional(12.0),
        visuals.text_color(),
        (rect.width() - 32.0).max(120.0),
    );
    let size = galley.size() + egui::vec2(20.0, 14.0);
    let card = egui::Rect::from_center_size(rect.center(), size);
    painter.rect_filled(card, 5.0, visuals.window_fill());
    painter.rect_stroke(card, 5.0, visuals.window_stroke(), egui::StrokeKind::Inside);
    painter.galley(
        card.min + egui::vec2(10.0, 7.0),
        galley,
        visuals.text_color(),
    );
}

fn satellite_activity_status(status: &str) -> Option<&str> {
    status
        .strip_prefix("Satellite: ")
        .map(str::trim)
        .filter(|activity| !activity.is_empty())
}

fn satellite_source_tab_width(available: f32, spacing: f32) -> f32 {
    panel_kit::chip_width(available, spacing, SatelliteSource::ALL.len())
}

pub(crate) fn nearest_sat_frame_for_time(
    runs: &[rw_ui::SatRunListing],
    preferred_key: Option<&rw_ui::SatRunKey>,
    target_utc: DateTime<Utc>,
) -> Option<(rw_ui::SatRunKey, u16)> {
    let anchor = preferred_key.or_else(|| runs.first().map(|run| &run.key))?;
    let anchor_family = sat_run_family(&anchor.run);
    let mut best: Option<(&rw_ui::SatRunListing, u16, i64)> = None;
    for run in runs.iter().filter(|run| {
        run.key.model == anchor.model && sat_run_family(&run.key.run) == anchor_family
    }) {
        for hhmm in run.frames.iter().copied() {
            let distance = sat_frame_distance_seconds(&run.key.run, hhmm, target_utc);
            if best.is_none_or(|(_, _, best_distance)| distance < best_distance) {
                best = Some((run, hhmm, distance));
            }
        }
    }
    best.map(|(run, hhmm, _)| (run.key.clone(), hhmm))
}

/// Frames to re-push after an IR-enhancement change: every frame of the
/// player's selected run, playhead first. The player panel caches COLORED
/// textures per scan time and `set_frame` replaces entries in place, so
/// re-requesting the whole run recolors a running loop without touching
/// its playback state — re-requesting only the playhead frame left every
/// other cached frame on the old palette, and the loop mixed palettes.
pub(crate) fn sat_enhancement_refresh_frames(
    runs: &[rw_ui::SatRunListing],
    selected_run: Option<&rw_ui::SatRunKey>,
    last_frame: Option<&(rw_ui::SatRunKey, u16)>,
) -> Vec<(rw_ui::SatRunKey, u16)> {
    let Some(key) = selected_run.or_else(|| last_frame.map(|(key, _)| key)) else {
        return Vec::new();
    };
    let playhead = match last_frame {
        Some((frame_key, hhmm)) if frame_key == key => Some(*hhmm),
        _ => None,
    };
    let Some(listing) = runs.iter().find(|run| &run.key == key) else {
        // The run list has not mirrored this run (yet): still refresh the
        // one frame known to be on screen.
        return playhead
            .map(|hhmm| vec![(key.clone(), hhmm)])
            .unwrap_or_default();
    };
    let mut frames = listing.frames.clone();
    if let Some(hhmm) = playhead
        && let Some(position) = frames.iter().position(|&frame| frame == hhmm)
    {
        // The visible frame recolors first.
        frames.remove(position);
        frames.insert(0, hhmm);
    }
    frames.into_iter().map(|hhmm| (key.clone(), hhmm)).collect()
}

pub(crate) fn sat_map_request_matches(
    request: &Option<(rw_ui::SatRunKey, u16)>,
    key: &rw_ui::SatRunKey,
    hhmm: u16,
) -> bool {
    request
        .as_ref()
        .is_some_and(|(pending_key, pending_hhmm)| pending_key == key && *pending_hhmm == hhmm)
}

fn satellite_run_key_is_other_source(key: &rw_ui::SatRunKey) -> bool {
    !matches!(key.model.as_str(), "g16" | "g17" | "g18" | "g19")
}

/// Match one stored run against the resolved GOES spec. Non-GOES sources
/// remain available in the shared player. GOES RGB composites are exempt
/// from only the single-band part of the prefix check, never from the selected
/// satellite or sector: admitting a cached GOES-West/other-sector composite
/// can show a healthy preview whose coverage is entirely outside the map.
fn satellite_run_key_matches_resolved_spec(
    key: &rw_ui::SatRunKey,
    model: &str,
    prefixes: &[String],
) -> bool {
    satellite_run_key_is_other_source(key)
        || key.model == model
            && if satellite_run_key_is_composite(key) {
                let run_sector = key.run.split_once("_rgb_").map(|(sector, _)| sector);
                prefixes.iter().any(|prefix| {
                    prefix
                        .rsplit_once("_c")
                        .map(|(sector, _)| {
                            run_sector.is_some_and(|run_sector| {
                                run_sector == sector
                                    || run_sector
                                        .strip_prefix(sector)
                                        .is_some_and(|suffix| suffix.starts_with("_win"))
                            })
                        })
                        .unwrap_or(false)
                })
            } else {
                prefixes
                    .iter()
                    .any(|prefix| key.run.starts_with(prefix.as_str()))
            }
}

/// True/natural-color RGB composite runs (`<sector>_rgb_<style>_<YYYYMMDD>`)
/// are exempt from the follow spec's BAND filter — they belong to a GOES
/// model dir but carry no single `c<band>` prefix. The selected satellite and
/// sector are still enforced by [`satellite_run_key_matches_resolved_spec`].
pub(crate) fn satellite_run_key_is_composite(key: &rw_ui::SatRunKey) -> bool {
    key.run.contains("_rgb_")
}

pub(crate) fn sat_run_family(run_name: &str) -> String {
    let mut saw_day = false;
    run_name
        .split('_')
        .filter(|token| {
            let is_day_token = token.len() == 8 && token.bytes().all(|byte| byte.is_ascii_digit());
            if is_day_token {
                saw_day = true;
                return false;
            }
            !(saw_day && token.bytes().all(|byte| byte.is_ascii_digit()))
        })
        .collect::<Vec<_>>()
        .join("_")
}

pub(crate) fn sat_frame_distance_seconds(
    run_name: &str,
    hhmm: u16,
    target_utc: DateTime<Utc>,
) -> i64 {
    if let Some(frame_time) = rw_sat::store::frame_time(run_name, hhmm) {
        return (frame_time - target_utc).num_seconds().abs();
    }
    let target_seconds = i64::from(target_utc.hour()) * 3600 + i64::from(target_utc.minute()) * 60;
    let frame_seconds = i64::from(hhmm / 100) * 3600 + i64::from(hhmm % 100) * 60;
    (frame_seconds - target_seconds).abs()
}

fn sat_player_frame_within_texture_limit(mut frame: rw_ui::SatFrameImage) -> SatFrameResizeResult {
    let old_size = frame.image.size;
    let max_dim = old_size[0].max(old_size[1]);
    if max_dim <= MAX_SAT_PLAYER_TEXTURE_DIM {
        return (frame, None);
    }
    let scale = MAX_SAT_PLAYER_TEXTURE_DIM as f32 / max_dim as f32;
    let new_size = [
        ((old_size[0] as f32 * scale).round() as usize).clamp(1, MAX_SAT_PLAYER_TEXTURE_DIM),
        ((old_size[1] as f32 * scale).round() as usize).clamp(1, MAX_SAT_PLAYER_TEXTURE_DIM),
    ];
    frame.image = resize_color_image_linear(&frame.image, new_size);
    (frame, Some((old_size, new_size)))
}

fn sample_sat_map_color(
    ctx: &SatMapSampleCtx<'_>,
    nearest_index: usize,
    target_lat: f32,
    target_lon: f32,
) -> egui::Color32 {
    let image = ctx.image;
    let grid = ctx.grid;
    let nx = ctx.nx;
    let ny = ctx.ny;
    let flip_rows = ctx.flip_rows;
    if nx < 2
        || ny < 2
        || nearest_index >= nx.saturating_mul(ny)
        || image.size != [nx, ny]
        || grid.nx != nx
        || grid.ny != ny
        || grid.lat.len() != nx.saturating_mul(ny)
        || grid.lon.len() != nx.saturating_mul(ny)
        || image.pixels.len() != nx.saturating_mul(ny)
    {
        return nearest_sat_map_color(image, nearest_index, nx, ny, flip_rows);
    }
    let row = nearest_index / nx;
    let col = nearest_index % nx;
    if let Some(color) = sample_sat_map_candidate_cells(ctx, row, col, target_lat, target_lon, 1) {
        return color;
    }
    if let Some(color) = sample_sat_map_candidate_cells(ctx, row, col, target_lat, target_lon, 3) {
        return color;
    }
    nearest_sat_map_color(image, nearest_index, nx, ny, flip_rows)
}

fn sample_sat_map_candidate_cells(
    ctx: &SatMapSampleCtx<'_>,
    row: usize,
    col: usize,
    target_lat: f32,
    target_lon: f32,
    radius: usize,
) -> Option<egui::Color32> {
    let max_y0 = ctx.ny.saturating_sub(2);
    let max_x0 = ctx.nx.saturating_sub(2);
    let y_min = row.saturating_sub(radius).min(max_y0);
    let y_max = row.saturating_add(radius).min(max_y0);
    let x_min = col.saturating_sub(radius).min(max_x0);
    let x_max = col.saturating_add(radius).min(max_x0);
    for y0 in y_min..=y_max {
        for x0 in x_min..=x_max {
            if let Some(color) = sample_sat_map_cell(ctx, x0, y0, target_lat, target_lon) {
                return Some(color);
            }
        }
    }
    None
}

fn sample_sat_map_cell(
    ctx: &SatMapSampleCtx<'_>,
    x0: usize,
    y0: usize,
    target_lat: f32,
    target_lon: f32,
) -> Option<egui::Color32> {
    let image = ctx.image;
    let grid = ctx.grid;
    let nx = ctx.nx;
    let ny = ctx.ny;
    let flip_rows = ctx.flip_rows;
    let i00 = y0 * nx + x0;
    let i10 = i00 + 1;
    let i01 = i00 + nx;
    let i11 = i01 + 1;
    let target_lon = f64::from(target_lon);
    let target_lat = f64::from(target_lat);
    let corners = [
        (
            model_layer::unwrap_lon_near(f64::from(*grid.lon.get(i00)?), target_lon),
            f64::from(*grid.lat.get(i00)?),
        ),
        (
            model_layer::unwrap_lon_near(f64::from(*grid.lon.get(i10)?), target_lon),
            f64::from(*grid.lat.get(i10)?),
        ),
        (
            model_layer::unwrap_lon_near(f64::from(*grid.lon.get(i01)?), target_lon),
            f64::from(*grid.lat.get(i01)?),
        ),
        (
            model_layer::unwrap_lon_near(f64::from(*grid.lon.get(i11)?), target_lon),
            f64::from(*grid.lat.get(i11)?),
        ),
    ];
    let (u, v) = model_layer::solve_bilinear_coords(corners, target_lon, target_lat)?;
    if !((-0.08..=1.08).contains(&u) && (-0.08..=1.08).contains(&v)) {
        return None;
    }
    let u = u.clamp(0.0, 1.0) as f32;
    let v = v.clamp(0.0, 1.0) as f32;
    let c00 = sat_map_grid_color(image, i00, nx, ny, flip_rows)?;
    let c10 = sat_map_grid_color(image, i10, nx, ny, flip_rows)?;
    let c01 = sat_map_grid_color(image, i01, nx, ny, flip_rows)?;
    let c11 = sat_map_grid_color(image, i11, nx, ny, flip_rows)?;
    Some(bilinear_color(c00, c10, c01, c11, u, v))
}

fn nearest_sat_map_color(
    image: &egui::ColorImage,
    index: usize,
    nx: usize,
    ny: usize,
    flip_rows: bool,
) -> egui::Color32 {
    sat_map_grid_color(image, index, nx, ny, flip_rows).unwrap_or(egui::Color32::TRANSPARENT)
}

fn sat_map_grid_color(
    image: &egui::ColorImage,
    index: usize,
    nx: usize,
    ny: usize,
    flip_rows: bool,
) -> Option<egui::Color32> {
    let row = index / nx;
    let col = index % nx;
    if row >= ny || col >= nx {
        return None;
    }
    let image_row = if flip_rows { ny - 1 - row } else { row };
    image.pixels.get(image_row * nx + col).copied()
}

fn bilinear_color(
    c00: egui::Color32,
    c10: egui::Color32,
    c01: egui::Color32,
    c11: egui::Color32,
    u: f32,
    v: f32,
) -> egui::Color32 {
    let weights = [(1.0 - u) * (1.0 - v), u * (1.0 - v), (1.0 - u) * v, u * v];
    let colors = [c00, c10, c01, c11];
    let mut r = 0.0;
    let mut g = 0.0;
    let mut b = 0.0;
    let mut a = 0.0;
    for (color, weight) in colors.into_iter().zip(weights) {
        r += f32::from(color.r()) * weight;
        g += f32::from(color.g()) * weight;
        b += f32::from(color.b()) * weight;
        a += f32::from(color.a()) * weight;
    }
    egui::Color32::from_rgba_unmultiplied(
        r.round().clamp(0.0, 255.0) as u8,
        g.round().clamp(0.0, 255.0) as u8,
        b.round().clamp(0.0, 255.0) as u8,
        a.round().clamp(0.0, 255.0) as u8,
    )
}

fn resize_color_image_linear(image: &egui::ColorImage, new_size: [usize; 2]) -> egui::ColorImage {
    let old_size = image.size;
    if old_size == new_size {
        return image.clone();
    }
    let (old_w, old_h) = (old_size[0], old_size[1]);
    let (new_w, new_h) = (new_size[0].max(1), new_size[1].max(1));
    if old_w == 0 || old_h == 0 || image.pixels.is_empty() {
        return egui::ColorImage::new(new_size, vec![egui::Color32::TRANSPARENT; new_w * new_h]);
    }
    let mut pixels = Vec::with_capacity(new_w * new_h);
    for y in 0..new_h {
        let src_y = if new_h <= 1 {
            0.0
        } else {
            y as f32 * (old_h - 1) as f32 / (new_h - 1) as f32
        };
        let y0 = src_y.floor() as usize;
        let y1 = (y0 + 1).min(old_h - 1);
        let v = src_y - y0 as f32;
        for x in 0..new_w {
            let src_x = if new_w <= 1 {
                0.0
            } else {
                x as f32 * (old_w - 1) as f32 / (new_w - 1) as f32
            };
            let x0 = src_x.floor() as usize;
            let x1 = (x0 + 1).min(old_w - 1);
            let u = src_x - x0 as f32;
            pixels.push(bilinear_color(
                image.pixels[y0 * old_w + x0],
                image.pixels[y0 * old_w + x1],
                image.pixels[y1 * old_w + x0],
                image.pixels[y1 * old_w + x1],
                u,
                v,
            ));
        }
    }
    egui::ColorImage::new([new_w, new_h], pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_goes_activity_only_accepts_satellite_status() {
        assert_eq!(
            satellite_activity_status("Satellite: Himawari B03 S04/10 · downloading 2.1 MB"),
            Some("Himawari B03 S04/10 · downloading 2.1 MB")
        );
        assert_eq!(satellite_activity_status("Radar: loading KTLX"), None);
        assert_eq!(satellite_activity_status("Satellite:   "), None);
    }

    #[test]
    fn satellite_source_tabs_split_the_row_three_ways() {
        assert_eq!(satellite_source_tab_width(520.0, 4.0), 170.0);
        assert_eq!(satellite_source_tab_width(320.0, 4.0), 104.0);
    }

    #[test]
    fn satellite_player_layout_reserves_a_practical_preview_at_any_pane_width() {
        let narrow = satellite_player_panel_height(320.0);
        let normal = satellite_player_panel_height(900.0);
        let wide = satellite_player_panel_height(2_000.0);

        assert_eq!(narrow, 352.0);
        assert_eq!(normal, 472.0);
        assert_eq!(wide, 532.0);
        assert!(
            narrow >= 300.0,
            "the native-frame preview must never collapse to a thumbnail"
        );
    }

    #[test]
    fn satellite_render_visibility_distinguishes_empty_and_covered_views() {
        let empty = egui::ColorImage::new([2, 2], vec![egui::Color32::TRANSPARENT; 4]);
        let covered = egui::ColorImage::new(
            [2, 2],
            vec![
                egui::Color32::TRANSPARENT,
                egui::Color32::TRANSPARENT,
                egui::Color32::from_rgb(3, 5, 8),
                egui::Color32::TRANSPARENT,
            ],
        );

        assert!(!satellite_render_has_visible_pixels(&empty));
        assert!(satellite_render_has_visible_pixels(&covered));
    }

    #[test]
    fn satellite_coverage_center_uses_a_visible_geolocated_pixel() {
        let mut pixels = vec![egui::Color32::TRANSPARENT; 9];
        // Grid index 7 is row 2/col 1. With flipped display rows, that lands
        // at image row 0/col 1.
        pixels[1] = egui::Color32::WHITE;
        let image = egui::ColorImage::new([3, 3], pixels);
        let grid = rw_store::grid::GridFile {
            nx: 3,
            ny: 3,
            lat: (0..9).map(|index| 30.0 + index as f32).collect(),
            lon: (0..9).map(|index| -120.0 + index as f32).collect(),
            projection: None,
            hash: String::new(),
        };

        assert_eq!(
            satellite_visible_coverage_point(&image, &grid, true),
            Some((37.0, -113.0))
        );
    }

    #[test]
    fn goes_composites_ignore_band_but_not_satellite_or_sector_filters() {
        let model = "g19";
        let prefixes = vec!["conus_c13_".to_owned()];
        let key = |model: &str, run: &str| rw_ui::SatRunKey {
            model: model.to_owned(),
            run: run.to_owned(),
        };

        assert!(satellite_run_key_matches_resolved_spec(
            &key("g19", "conus_rgb_natural_color_20260713"),
            model,
            &prefixes,
        ));
        assert!(satellite_run_key_matches_resolved_spec(
            &key("g19", "conus_win295n954w600_rgb_natural_color_20260713"),
            model,
            &prefixes,
        ));
        assert!(
            !satellite_run_key_matches_resolved_spec(
                &key("g19", "fulldisk_rgb_natural_color_20260713"),
                model,
                &prefixes,
            ),
            "a composite must not bypass the selected sector"
        );
        assert!(
            !satellite_run_key_matches_resolved_spec(
                &key("g18", "conus_rgb_natural_color_20260713"),
                model,
                &prefixes,
            ),
            "a cached GOES-West composite must not appear under a GOES-East spec"
        );
        assert!(!satellite_run_key_matches_resolved_spec(
            &key("g19", "conus_c02_20260713"),
            model,
            &prefixes,
        ));
        assert!(satellite_run_key_matches_resolved_spec(
            &key("h9", "fulldisk_rgb_true_color_20260713"),
            model,
            &prefixes,
        ));
    }

    #[test]
    fn live_goes_frame_refresh_scans_and_selects_the_just_written_run() {
        match satellite_frame_written_refresh_request(
            "g18".to_owned(),
            "conus_c01_20260713".to_owned(),
            1851,
            true,
        ) {
            sat_worker::SatRequest::ScanAndSelect { key, hhmm } => {
                assert_eq!(key.model, "g18");
                assert_eq!(key.run, "conus_c01_20260713");
                assert_eq!(hhmm, 1851);
            }
            other => panic!("expected live GOES scan-and-select, got {other:?}"),
        }

        assert!(matches!(
            satellite_frame_written_refresh_request(
                "g18".to_owned(),
                "conus_rgb_natural_color_20260713".to_owned(),
                1851,
                false,
            ),
            sat_worker::SatRequest::Scan
        ));
    }

    #[test]
    fn satellite_player_preview_is_capped_below_wgpu_texture_limit() {
        let frame = rw_ui::SatFrameImage {
            key: rw_ui::SatRunKey {
                model: "g19".to_owned(),
                run: "fulldisk_c13_20260614".to_owned(),
            },
            hhmm: 1850,
            image: egui::ColorImage::new([10_000, 16], vec![egui::Color32::WHITE; 10_000 * 16]),
            read_ms: 1.0,
        };

        let (frame, resized) = sat_player_frame_within_texture_limit(frame);

        assert_eq!(resized, Some(([10_000, 16], [4096, 7])));
        assert_eq!(frame.image.size, [MAX_SAT_PLAYER_TEXTURE_DIM, 7]);
        assert_eq!(frame.image.pixels.len(), MAX_SAT_PLAYER_TEXTURE_DIM * 7);
    }

    #[test]
    fn satellite_map_sampler_interpolates_source_pixels() {
        let image = egui::ColorImage::new(
            [2, 2],
            vec![
                egui::Color32::from_rgb(0, 0, 0),
                egui::Color32::from_rgb(100, 0, 0),
                egui::Color32::from_rgb(200, 0, 0),
                egui::Color32::from_rgb(252, 0, 0),
            ],
        );
        let grid = rw_store::grid::GridFile {
            nx: 2,
            ny: 2,
            lat: vec![40.0, 40.0, 41.0, 41.0],
            lon: vec![-100.0, -99.0, -100.0, -99.0],
            projection: None,
            hash: "sat-test".to_owned(),
        };

        let sample_ctx = SatMapSampleCtx {
            image: &image,
            grid: &grid,
            nx: 2,
            ny: 2,
            flip_rows: false,
        };
        let color = sample_sat_map_color(&sample_ctx, 0, 40.5, -99.5);

        assert!(
            (i16::from(color.r()) - 138).abs() <= 1,
            "expected midpoint red channel, got {color:?}"
        );
    }

    #[test]
    fn satellite_map_sampler_searches_nearby_cells_when_lut_neighbor_misses() {
        let mut pixels = vec![egui::Color32::BLACK; 16];
        for index in [10, 11, 14, 15] {
            pixels[index] = egui::Color32::from_rgb(120, 0, 0);
        }
        let image = egui::ColorImage::new([4, 4], pixels);
        let mut lat = Vec::new();
        let mut lon = Vec::new();
        for y in 0..4 {
            for x in 0..4 {
                lat.push(y as f32);
                lon.push(x as f32);
            }
        }
        let grid = rw_store::grid::GridFile {
            nx: 4,
            ny: 4,
            lat,
            lon,
            projection: None,
            hash: "sat-test".to_owned(),
        };

        let sample_ctx = SatMapSampleCtx {
            image: &image,
            grid: &grid,
            nx: 4,
            ny: 4,
            flip_rows: false,
        };
        let color = sample_sat_map_color(&sample_ctx, 0, 2.5, 2.5);

        assert_eq!(
            color.r(),
            120,
            "should find the containing source cell instead of falling back to the wrong nearest pixel"
        );
    }
}
