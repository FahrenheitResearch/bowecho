use chrono::{DateTime, Duration, NaiveDate, Utc};
use eframe::egui;

#[derive(Clone, Debug)]
pub(crate) struct UnifiedPlayerState {
    pub(crate) open: bool,
    pub(crate) auto_sync_warnings: bool,
    pub(crate) start_date_input: String,
    pub(crate) start_hour_input: String,
    pub(crate) start_minute_input: String,
    pub(crate) end_date_input: String,
    pub(crate) end_hour_input: String,
    pub(crate) end_minute_input: String,
    pub(crate) coordinated_sites_input: String,
    pub(crate) coordinated_site_radius_km: f32,
    status: String,
}

#[derive(Clone, Debug)]
pub(crate) struct UnifiedPlayerContext {
    pub(crate) source_label: String,
    pub(crate) load_busy: bool,
    pub(crate) frame_count: usize,
    pub(crate) timeline_step_count: usize,
    pub(crate) selected_step_index: usize,
    pub(crate) can_play_timeline: bool,
    pub(crate) history_playing: bool,
    pub(crate) selected_time_utc: Option<DateTime<Utc>>,
    pub(crate) loop_start_utc: Option<DateTime<Utc>>,
    pub(crate) loop_end_utc: Option<DateTime<Utc>>,
    pub(crate) history_frame_limit: usize,
    pub(crate) history_frame_limit_max: usize,
    pub(crate) history_frame_limit_options: Vec<usize>,
    pub(crate) loop_speed_percent: u16,
    pub(crate) loop_speed_options: Vec<u16>,
    pub(crate) low_sweeps_enabled: bool,
    pub(crate) low_sweep_mode_label: String,
    pub(crate) low_sweep_filter_index: usize,
    pub(crate) low_sweep_filter_options: Vec<String>,
    pub(crate) auto_sync_warnings: bool,
    pub(crate) warnings_synced_window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    pub(crate) warnings_loaded: bool,
    pub(crate) warnings_loading: bool,
    pub(crate) warnings_timeline_ready: bool,
    pub(crate) warnings_need_sync: bool,
    pub(crate) spc_reports_enabled: bool,
    pub(crate) mping_enabled: bool,
    pub(crate) reports_timeline_time_utc: Option<DateTime<Utc>>,
    pub(crate) satellite_map_follow: bool,
    pub(crate) satellite_frame_label: Option<String>,
    pub(crate) satellite_run_count: usize,
    pub(crate) model_enabled: bool,
    pub(crate) model_timeline_follow: bool,
    pub(crate) model_frame_label: Option<String>,
    pub(crate) camera_follow_label: Option<String>,
    pub(crate) storm_tracks_enabled: bool,
    pub(crate) storm_follow_active: bool,
    pub(crate) storm_follow_lead_index: usize,
    pub(crate) storm_follow_lead_options: Vec<String>,
    pub(crate) manual_camera_keyframes: usize,
    pub(crate) manual_camera_can_follow: bool,
    pub(crate) manual_camera_follow: bool,
    pub(crate) hide_camera_guides: bool,
    pub(crate) loop_recording: bool,
    pub(crate) free_recording: bool,
    pub(crate) can_record_loop: bool,
    pub(crate) full_resolution_export: bool,
    pub(crate) record_settings_label: String,
    pub(crate) docked: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum UnifiedPlayerAction {
    LoadLatest,
    LoadLoop,
    LoadArchiveEndingAt,
    LoadArchiveWindow,
    PreviousFrame,
    TogglePlayback,
    NextFrame,
    SelectTimelineStep(usize),
    SetHistoryFrameLimit(usize),
    SetLoopSpeedPercent(u16),
    SetLowSweepsEnabled(bool),
    SetLowSweepFilter(usize),
    OpenSweepControls,
    SetAutoWarningSync(bool),
    SyncWarningsToLoop,
    ReleaseWarningSync,
    SetSpcReportsEnabled(bool),
    SetMpingEnabled(bool),
    SetSatelliteMapFollow(bool),
    SetModelTimelineFollow(bool),
    SetStormTracksEnabled(bool),
    AutoFollowStrongestStorm,
    SetStormFollowLead(usize),
    StopStormFollow,
    AddManualCameraKeyframe,
    SetManualCameraFollow(bool),
    SetHideCameraGuides(bool),
    ClearManualCameraPath,
    ClearCameraFollow,
    UseFullResolutionExportPreset,
    ToggleLoopRecording,
    ToggleFreeRecording,
    FindNearbySites,
    AddCoordinatedSitesAsOverlays,
    SyncNearbyRadarLoops,
    Dock,
}

impl Default for UnifiedPlayerState {
    fn default() -> Self {
        Self {
            open: false,
            auto_sync_warnings: false,
            start_date_input: String::new(),
            start_hour_input: String::new(),
            start_minute_input: String::new(),
            end_date_input: String::new(),
            end_hour_input: String::new(),
            end_minute_input: String::new(),
            coordinated_sites_input: String::new(),
            coordinated_site_radius_km: 230.0,
            status: String::new(),
        }
    }
}

impl UnifiedPlayerState {
    pub(crate) fn mark_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    pub(crate) fn archive_end_time_utc(&self) -> Result<DateTime<Utc>, String> {
        parse_utc_endpoint(
            &self.end_date_input,
            &self.end_hour_input,
            &self.end_minute_input,
            "End",
        )
    }

    pub(crate) fn archive_window_utc(&self) -> Result<(DateTime<Utc>, DateTime<Utc>), String> {
        let start = parse_utc_endpoint(
            &self.start_date_input,
            &self.start_hour_input,
            &self.start_minute_input,
            "Start",
        )?;
        let end = self.archive_end_time_utc()?;
        if end <= start {
            return Err("End time must be after start time".to_owned());
        }
        Ok((start, end))
    }

    fn set_start_time(&mut self, time: DateTime<Utc>) {
        self.start_date_input = time.format("%Y-%m-%d").to_string();
        self.start_hour_input = time.format("%H").to_string();
        self.start_minute_input = time.format("%M").to_string();
    }

    fn set_end_time(&mut self, time: DateTime<Utc>) {
        self.end_date_input = time.format("%Y-%m-%d").to_string();
        self.end_hour_input = time.format("%H").to_string();
        self.end_minute_input = time.format("%M").to_string();
    }

    fn ensure_time_inputs(&mut self, context: &UnifiedPlayerContext) {
        let anchor_end = context
            .loop_end_utc
            .or(context.selected_time_utc)
            .unwrap_or_else(Utc::now);
        let anchor_start = context
            .loop_start_utc
            .unwrap_or_else(|| anchor_end - Duration::hours(4));
        if self.start_date_input.trim().is_empty()
            || self.start_hour_input.trim().is_empty()
            || self.start_minute_input.trim().is_empty()
        {
            self.set_start_time(anchor_start);
        }
        if self.end_date_input.trim().is_empty()
            || self.end_hour_input.trim().is_empty()
            || self.end_minute_input.trim().is_empty()
        {
            self.set_end_time(anchor_end);
        }
    }
}

fn parse_utc_endpoint(
    date_input: &str,
    hour_input: &str,
    minute_input: &str,
    label: &str,
) -> Result<DateTime<Utc>, String> {
    let date = NaiveDate::parse_from_str(date_input.trim(), "%Y-%m-%d")
        .map_err(|_| format!("{label} date must be YYYY-MM-DD"))?;
    let hour: u32 = hour_input
        .trim()
        .parse()
        .map_err(|_| format!("{label} hour must be 0-23 UTC"))?;
    if hour > 23 {
        return Err(format!("{label} hour must be 0-23 UTC"));
    }
    let minute = if minute_input.trim().is_empty() {
        0
    } else {
        let minute: u32 = minute_input
            .trim()
            .parse()
            .map_err(|_| format!("{label} minute must be 0-59"))?;
        if minute > 59 {
            return Err(format!("{label} minute must be 0-59"));
        }
        minute
    };
    let naive = date
        .and_hms_opt(hour, minute, 0)
        .ok_or_else(|| format!("{label} time is invalid"))?;
    Ok(DateTime::from_naive_utc_and_offset(naive, Utc))
}

impl UnifiedPlayerState {
    pub(crate) fn body_ui(
        &mut self,
        ui: &mut egui::Ui,
        context: &UnifiedPlayerContext,
    ) -> Option<UnifiedPlayerAction> {
        let mut action = None;
        self.ensure_time_inputs(context);
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.heading("Unified Player");
                ui.separator();
                ui.label(loop_label(context));
                if context.load_busy {
                    ui.label(egui::RichText::new("loading").weak());
                }
                if !context.docked
                    && stable_button(ui, "Dock", 62.0, true)
                        .on_hover_text("Dock this player into the main workspace")
                        .clicked()
                {
                    action = Some(UnifiedPlayerAction::Dock);
                }
            });
            ui.label(egui::RichText::new(&context.source_label).weak());
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                if stable_button(ui, "<", 38.0, context.timeline_step_count > 1)
                    .on_hover_text("Previous timeline step")
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::PreviousFrame);
                }
                let play_label = if context.history_playing { "Pause" } else { "Play" };
                if stable_button(ui, play_label, 86.0, context.can_play_timeline)
                    .on_hover_text("Play loaded timeline")
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::TogglePlayback);
                }
                if stable_button(ui, ">", 38.0, context.timeline_step_count > 1)
                    .on_hover_text("Next timeline step")
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::NextFrame);
                }
                ui.label(egui::RichText::new(frame_counter_label(context)).strong());
                ui.separator();
                ui.label(selected_time_label(context.selected_time_utc));
            });

            ui.add_space(4.0);
            let slider_max = context.timeline_step_count.saturating_sub(1);
            let mut slider_index = context.selected_step_index.min(slider_max);
            let slider_response = ui
                .add_enabled_ui(context.timeline_step_count > 1, |ui| {
                    ui.add_sized(
                        egui::vec2(ui.available_width(), 36.0),
                        egui::Slider::new(&mut slider_index, 0..=slider_max).show_value(false),
                    )
                })
                .inner
                .on_hover_text("Scrub the loaded event timeline");
            if slider_response.changed() {
                action = Some(UnifiedPlayerAction::SelectTimelineStep(slider_index));
            }

            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                if stable_button(ui, "Load Latest", 104.0, true)
                    .on_hover_text("Load the newest frame for the selected radar")
                    .clicked()
                {
                    if context.load_busy {
                        self.mark_status("A radar load is already running");
                    } else {
                        action = Some(UnifiedPlayerAction::LoadLatest);
                    }
                }
                if stable_button(ui, "Load Loop", 112.0, true)
                    .on_hover_text("Load a loop using the frame count below")
                    .clicked()
                {
                    if context.load_busy {
                        self.mark_status("A radar load is already running");
                    } else {
                        action = Some(UnifiedPlayerAction::LoadLoop);
                    }
                }
                if stable_button(ui, "Archive Window", 126.0, true)
                    .on_hover_text(
                        "Load a US/ORD archive loop for the explicit UTC start/end below; never routes to live/latest",
                    )
                    .clicked()
                {
                    if context.load_busy {
                        self.mark_status("A radar load is already running");
                    } else {
                        action = Some(UnifiedPlayerAction::LoadArchiveWindow);
                    }
                }
                if stable_button(ui, "Loop Ending At", 112.0, true)
                    .on_hover_text(
                        "Load an archive loop ending at the UTC time below (not a centered event loop)",
                    )
                    .clicked()
                {
                    if context.load_busy {
                        self.mark_status("A radar load is already running");
                    } else {
                        action = Some(UnifiedPlayerAction::LoadArchiveEndingAt);
                    }
                }
            });

            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Frames");
                let mut selected_limit = context.history_frame_limit;
                egui::ComboBox::from_id_salt("unified_player_frame_limit")
                    .selected_text(context.history_frame_limit.to_string())
                    .width(64.0)
                    .show_ui(ui, |ui| {
                        for limit in &context.history_frame_limit_options {
                            ui.selectable_value(&mut selected_limit, *limit, limit.to_string());
                        }
                    });
                if selected_limit != context.history_frame_limit {
                    action = Some(UnifiedPlayerAction::SetHistoryFrameLimit(selected_limit));
                }
                let mut typed_limit = context.history_frame_limit;
                if ui
                    .add_sized(
                        egui::vec2(82.0, CONTROL_HEIGHT),
                        egui::DragValue::new(&mut typed_limit)
                            .range(1..=context.history_frame_limit_max)
                            .speed(1.0)
                            .prefix("N "),
                    )
                    .on_hover_text("Requested/kept radar frames for long loops")
                    .changed()
                {
                    action = Some(UnifiedPlayerAction::SetHistoryFrameLimit(typed_limit));
                }

                ui.separator();
                ui.label("Speed");
                let mut speed = context.loop_speed_percent;
                egui::ComboBox::from_id_salt("unified_player_speed")
                    .selected_text(loop_speed_label(speed))
                    .width(72.0)
                    .show_ui(ui, |ui| {
                        for option in &context.loop_speed_options {
                            ui.selectable_value(&mut speed, *option, loop_speed_label(*option));
                        }
                    });
                if speed != context.loop_speed_percent {
                    action = Some(UnifiedPlayerAction::SetLoopSpeedPercent(speed));
                }

                ui.separator();
                let mut low_sweeps = context.low_sweeps_enabled;
                if ui
                    .checkbox(&mut low_sweeps, "Low sweeps")
                    .on_hover_text("Step complete low-level SAILS/terminal sweeps inside each scan")
                    .changed()
                {
                    action = Some(UnifiedPlayerAction::SetLowSweepsEnabled(low_sweeps));
                }
                if low_sweeps && !context.low_sweep_filter_options.is_empty() {
                    let mut filter_index = context
                        .low_sweep_filter_index
                        .min(context.low_sweep_filter_options.len() - 1);
                    egui::ComboBox::from_id_salt("unified_player_low_sweep_filter")
                        .selected_text(&context.low_sweep_mode_label)
                        .width(112.0)
                        .show_ui(ui, |ui| {
                            for (index, label) in
                                context.low_sweep_filter_options.iter().enumerate()
                            {
                                ui.selectable_value(&mut filter_index, index, label);
                            }
                        });
                    if filter_index != context.low_sweep_filter_index {
                        action = Some(UnifiedPlayerAction::SetLowSweepFilter(filter_index));
                    }
                }
                if stable_button(ui, "⚙", 34.0, true)
                    .on_hover_text("Product- and pane-specific sweep controls")
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::OpenSweepControls);
                }
            });

            ui.separator();
            egui::Grid::new("unified_player_loop_grid")
                .num_columns(2)
                .spacing([10.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Radar");
                    ui.label(format!("{} frame(s)", context.frame_count));
                    ui.end_row();

                    ui.label("Selected");
                    ui.label(format_time(context.selected_time_utc));
                    ui.end_row();

                    ui.label("Range");
                    ui.label(format_range(context.loop_start_utc, context.loop_end_utc));
                    ui.end_row();
                });
            egui::Grid::new("unified_player_archive_window_grid")
                .num_columns(4)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Start UTC");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.start_date_input)
                            .desired_width(90.0)
                            .hint_text("YYYY-MM-DD"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.start_hour_input)
                            .desired_width(30.0)
                            .hint_text("HH"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.start_minute_input)
                            .desired_width(30.0)
                            .hint_text("MM"),
                    );
                    ui.end_row();

                    ui.label("End UTC");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.end_date_input)
                            .desired_width(90.0)
                            .hint_text("YYYY-MM-DD"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.end_hour_input)
                            .desired_width(30.0)
                            .hint_text("HH"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.end_minute_input)
                            .desired_width(30.0)
                            .hint_text("MM"),
                    );
                    ui.end_row();
                });
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_sized(
                        egui::vec2(96.0, CONTROL_HEIGHT),
                        egui::Button::new("Use selected"),
                    )
                    .on_hover_text("Copy the current frame time into the end-time controls")
                    .clicked()
                    && let Some(time) = context.selected_time_utc
                {
                    self.set_end_time(time);
                }
                if ui
                    .add_sized(
                        egui::vec2(126.0, CONTROL_HEIGHT),
                        egui::Button::new("Use loaded range"),
                    )
                    .on_hover_text("Copy the currently loaded loop range into the start/end controls")
                    .clicked()
                    && let (Some(start), Some(end)) = (context.loop_start_utc, context.loop_end_utc)
                {
                    self.set_start_time(start);
                    self.set_end_time(end);
                }
            });

            ui.separator();
            ui.label(egui::RichText::new("Export").strong());
            ui.horizontal_wrapped(|ui| {
                let loop_label = if context.loop_recording {
                    "Stop loop"
                } else {
                    "Record loop"
                };
                let loop_enabled =
                    context.loop_recording || (!context.free_recording && context.can_record_loop);
                if stable_button(
                    ui,
                    if context.full_resolution_export {
                        "Native MP4 on"
                    } else {
                        "Native MP4"
                    },
                    118.0,
                    !context.loop_recording && !context.free_recording,
                )
                    .on_hover_text(
                        "Use full physical-pixel capture resolution and MP4 for high-quality timeline exports",
                    )
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::UseFullResolutionExportPreset);
                }
                if stable_button(ui, loop_label, 104.0, loop_enabled)
                    .on_hover_text(
                        "Record one deterministic timeline cycle to GIF/MP4 at the current recording size",
                    )
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::ToggleLoopRecording);
                }
                let free_label = if context.free_recording {
                    "Stop free"
                } else {
                    "Free record"
                };
                if stable_button(ui, free_label, 104.0, !context.loop_recording)
                    .on_hover_text(
                        "Start/stop a full-window recording while you pan, zoom, scrub, and move the app",
                    )
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::ToggleFreeRecording);
                }
                ui.weak(&context.record_settings_label);
            });

            ui.separator();
            ui.label(egui::RichText::new("Radar Sources").strong());
            ui.horizontal_wrapped(|ui| {
                ui.label("Sites");
                ui.add(
                    egui::TextEdit::singleline(&mut self.coordinated_sites_input)
                        .desired_width(220.0)
                        .hint_text("KTLX,KINX,KFDR"),
                )
                .on_hover_text(
                    "Comma or space separated radar IDs. If a loop is loaded, added overlays sync to that loop automatically.",
                );
                ui.label("Radius");
                ui.add(
                    egui::DragValue::new(&mut self.coordinated_site_radius_km)
                        .range(25.0..=460.0)
                        .speed(5.0)
                        .suffix(" km"),
                )
                .on_hover_text("Nearby-site search radius for coordinated loop planning");
                if stable_button(ui, "Find nearby", 92.0, true).clicked() {
                    action = Some(UnifiedPlayerAction::FindNearbySites);
                }
                if stable_button(ui, "Add/sync", 92.0, true)
                    .on_hover_text("Add these radar overlays. Loaded loops are synced automatically.")
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::AddCoordinatedSitesAsOverlays);
                }
                if stable_button(ui, "Nearest 4", 86.0, true)
                    .on_hover_text(
                        "Fill the Sites box with nearby WSR-88D radars and add them as synced overlays for the loaded loop",
                    )
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::SyncNearbyRadarLoops);
                }
            });

            ui.separator();
            ui.label(egui::RichText::new("Camera").strong());
            if let Some(label) = &context.camera_follow_label {
                ui.weak(format!("Following {label}"));
            } else {
                ui.weak("No camera follow active");
            }
            ui.horizontal_wrapped(|ui| {
                let mut storm_tracks = context.storm_tracks_enabled;
                if ui
                    .checkbox(&mut storm_tracks, "Storm tracks")
                    .on_hover_text("Enable SCIT-style storm-cell tracks for follow-camera playback")
                    .changed()
                {
                    action = Some(UnifiedPlayerAction::SetStormTracksEnabled(storm_tracks));
                }
                if ui
                    .add_sized(
                        egui::vec2(126.0, CONTROL_HEIGHT),
                        egui::Button::new("Follow strongest"),
                    )
                    .on_hover_text("Pick the strongest current storm track and follow it with continuous between-scan camera motion")
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::AutoFollowStrongestStorm);
                }
                if context.storm_follow_active {
                    let mut lead_index = context
                        .storm_follow_lead_index
                        .min(context.storm_follow_lead_options.len().saturating_sub(1));
                    egui::ComboBox::from_id_salt("unified_player_storm_follow_lead_model")
                        .selected_text(
                            context
                                .storm_follow_lead_options
                                .get(lead_index)
                                .cloned()
                                .unwrap_or_else(|| "current".to_owned()),
                        )
                        .width(82.0)
                        .show_ui(ui, |ui| {
                            for (index, label) in
                                context.storm_follow_lead_options.iter().enumerate()
                            {
                                ui.selectable_value(&mut lead_index, index, label);
                            }
                        });
                    if lead_index != context.storm_follow_lead_index {
                        action = Some(UnifiedPlayerAction::SetStormFollowLead(lead_index));
                    }
                    if stable_button(ui, "Stop", 52.0, true).clicked() {
                        action = Some(UnifiedPlayerAction::StopStormFollow);
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Manual path: {} point(s)", context.manual_camera_keyframes));
                if ui
                    .add_sized(
                        egui::vec2(96.0, CONTROL_HEIGHT),
                        egui::Button::new("Mark center"),
                    )
                    .on_hover_text(
                        "Add/replace a manual camera keyframe at the current timeline time and map center",
                    )
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::AddManualCameraKeyframe);
                }
                let mut manual_follow = context.manual_camera_follow;
                if ui
                    .add_enabled(
                        context.manual_camera_can_follow,
                        egui::Checkbox::new(&mut manual_follow, "Follow path"),
                    )
                    .on_hover_text("Interpolate map center between manual keyframes during playback/export")
                    .changed()
                {
                    action = Some(UnifiedPlayerAction::SetManualCameraFollow(manual_follow));
                }
                let mut hide_guides = context.hide_camera_guides;
                if ui
                    .checkbox(&mut hide_guides, "Hide guides")
                    .on_hover_text("Hide storm/tornado guide lines while any camera follow mode is active")
                    .changed()
                {
                    action = Some(UnifiedPlayerAction::SetHideCameraGuides(hide_guides));
                }
                if stable_button(ui, "Clear path", 82.0, true).clicked() {
                    action = Some(UnifiedPlayerAction::ClearManualCameraPath);
                }
                if stable_button(ui, "Clear follow", 96.0, context.camera_follow_label.is_some())
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::ClearCameraFollow);
                }
            });

            ui.separator();
            ui.label(egui::RichText::new("Warnings").strong());
            ui.horizontal_wrapped(|ui| {
                let mut auto_sync = context.auto_sync_warnings;
                if ui
                    .checkbox(&mut auto_sync, "Auto sync archive")
                    .on_hover_text(
                        "Keep warning polygons tied to archive loops. Live-follow radar always keeps current warnings",
                    )
                    .changed()
                {
                    action = Some(UnifiedPlayerAction::SetAutoWarningSync(auto_sync));
                }
                if stable_button(ui, "Sync warnings", 112.0, context.can_sync_warnings())
                    .on_hover_text("Load warning polygons for the loaded loop range")
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::SyncWarningsToLoop);
                }
                if stable_button(
                    ui,
                    "Live warnings",
                    112.0,
                    context.warnings_synced_window.is_some(),
                )
                    .on_hover_text("Return warning polygons to live/current mode")
                    .clicked()
                {
                    action = Some(UnifiedPlayerAction::ReleaseWarningSync);
                }
            });
            let warnings_text = if context.warnings_loading {
                "loading".to_owned()
            } else if context.warnings_timeline_ready {
                "timeline ready".to_owned()
            } else if context.warnings_need_sync {
                "timeline needs sync".to_owned()
            } else if let Some((start, end)) = context.warnings_synced_window {
                format!(
                    "timeline {} to {}",
                    start.format("%H:%MZ"),
                    end.format("%H:%MZ")
                )
            } else if context.warnings_loaded {
                "live/current".to_owned()
            } else {
                "not loaded".to_owned()
            };
            ui.label(format!("Warnings: {warnings_text}"));
            ui.horizontal_wrapped(|ui| {
                let mut spc_reports = context.spc_reports_enabled;
                if ui
                    .checkbox(&mut spc_reports, "SPC reports")
                    .on_hover_text("Show SPC storm reports filtered to the current timeline time")
                    .changed()
                {
                    action = Some(UnifiedPlayerAction::SetSpcReportsEnabled(spc_reports));
                }
                let mut mping = context.mping_enabled;
                if ui
                    .checkbox(&mut mping, "mPING")
                    .on_hover_text("Show mPING public reports filtered to the current timeline time")
                    .changed()
                {
                    action = Some(UnifiedPlayerAction::SetMpingEnabled(mping));
                }
            });
            let reports_text = if !context.spc_reports_enabled && !context.mping_enabled {
                "off".to_owned()
            } else if let Some(time) = context.reports_timeline_time_utc {
                format!("timeline {}", time.format("%H:%MZ"))
            } else {
                "live/current".to_owned()
            };
            ui.label(format!("Reports: {reports_text}"));
            ui.horizontal_wrapped(|ui| {
                let mut satellite_follow = context.satellite_map_follow;
                if ui
                    .checkbox(&mut satellite_follow, "Satellite map follows timeline")
                    .on_hover_text(
                        "Drive the radar-map satellite overlay to the nearest stored satellite frame for the current loop time",
                    )
                    .changed()
                {
                    action = Some(UnifiedPlayerAction::SetSatelliteMapFollow(
                        satellite_follow,
                    ));
                }
                let satellite_text = context
                    .satellite_frame_label
                    .clone()
                    .unwrap_or_else(|| {
                        if context.satellite_run_count > 0 {
                            format!("{} run(s) indexed", context.satellite_run_count)
                        } else {
                            "no indexed satellite frames".to_owned()
                        }
                });
                ui.weak(format!("Satellite: {satellite_text}"));
            });
            ui.horizontal_wrapped(|ui| {
                let mut model_follow = context.model_timeline_follow;
                if ui
                    .add_enabled_ui(context.model_enabled || model_follow, |ui| {
                        ui.checkbox(&mut model_follow, "Model follows timeline")
                    })
                    .inner
                    .on_hover_text(
                        "Select the newest model forecast hour valid closest to the current loop time",
                    )
                    .changed()
                {
                    action = Some(UnifiedPlayerAction::SetModelTimelineFollow(model_follow));
                }
                let model_text = context.model_frame_label.clone().unwrap_or_else(|| {
                    if context.model_enabled {
                        "no selected model hour".to_owned()
                    } else {
                        "model layer off".to_owned()
                    }
                });
                ui.weak(format!("Model: {model_text}"));
            });

            if !self.status.is_empty() {
                ui.separator();
                ui.label(egui::RichText::new(&self.status).strong());
            }
        });
        action
    }
}

const CONTROL_HEIGHT: f32 = 24.0;

fn stable_button(ui: &mut egui::Ui, label: &str, width: f32, enabled: bool) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        ui.add_sized(egui::vec2(width, CONTROL_HEIGHT), egui::Button::new(label))
    })
    .inner
}

impl UnifiedPlayerContext {
    pub(crate) fn can_sync_warnings(&self) -> bool {
        self.timeline_step_count > 0 && self.loop_start_utc.is_some() && self.loop_end_utc.is_some()
    }
}

impl Default for UnifiedPlayerContext {
    fn default() -> Self {
        Self {
            source_label: "No source".to_owned(),
            load_busy: false,
            frame_count: 0,
            timeline_step_count: 0,
            selected_step_index: 0,
            can_play_timeline: false,
            history_playing: false,
            selected_time_utc: None,
            loop_start_utc: None,
            loop_end_utc: None,
            history_frame_limit: 10,
            history_frame_limit_max: 2000,
            history_frame_limit_options: vec![
                3, 5, 7, 10, 15, 20, 25, 30, 48, 72, 96, 128, 160, 200, 256, 384, 512, 768, 1000,
                1500, 2000,
            ],
            loop_speed_percent: 100,
            loop_speed_options: vec![25, 50, 100, 200, 400, 800, 1600, 3200, 6400],
            low_sweeps_enabled: false,
            low_sweep_mode_label: "All low tilts".to_owned(),
            low_sweep_filter_index: 0,
            low_sweep_filter_options: vec![
                "All low tilts".to_owned(),
                "Same level".to_owned(),
                "Base only".to_owned(),
            ],
            auto_sync_warnings: false,
            warnings_synced_window: None,
            warnings_loaded: false,
            warnings_loading: false,
            warnings_timeline_ready: false,
            warnings_need_sync: false,
            spc_reports_enabled: false,
            mping_enabled: false,
            reports_timeline_time_utc: None,
            satellite_map_follow: false,
            satellite_frame_label: None,
            satellite_run_count: 0,
            model_enabled: false,
            model_timeline_follow: false,
            model_frame_label: None,
            camera_follow_label: None,
            storm_tracks_enabled: false,
            storm_follow_active: false,
            storm_follow_lead_index: 0,
            storm_follow_lead_options: vec![
                "current".to_owned(),
                "+15 min".to_owned(),
                "+30 min".to_owned(),
                "+45 min".to_owned(),
            ],
            manual_camera_keyframes: 0,
            manual_camera_can_follow: false,
            manual_camera_follow: false,
            hide_camera_guides: false,
            loop_recording: false,
            free_recording: false,
            can_record_loop: false,
            full_resolution_export: false,
            record_settings_label: "720 Auto · free 30fps".to_owned(),
            docked: false,
        }
    }
}

fn loop_label(context: &UnifiedPlayerContext) -> String {
    match (context.timeline_step_count, context.frame_count) {
        (0, _) => "No loop loaded".to_owned(),
        (steps, frames) if steps != frames => format!("{steps} steps / {frames} frames"),
        (1, _) => "1 frame".to_owned(),
        (count, _) => format!("{count} frames"),
    }
}

fn frame_counter_label(context: &UnifiedPlayerContext) -> String {
    if context.timeline_step_count == 0 {
        "none".to_owned()
    } else {
        format!(
            "{} / {}",
            context.selected_step_index.saturating_add(1),
            context.timeline_step_count
        )
    }
}

fn selected_time_label(time: Option<DateTime<Utc>>) -> String {
    time.map(|time| time.format("%Y-%m-%d %H:%M:%SZ").to_string())
        .unwrap_or_else(|| "No selected frame".to_owned())
}

fn format_time(time: Option<DateTime<Utc>>) -> String {
    time.map(|time| time.format("%Y-%m-%d %H:%M:%SZ").to_string())
        .unwrap_or_else(|| "none".to_owned())
}

fn format_range(start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>) -> String {
    match (start, end) {
        (Some(start), Some(end)) => format!(
            "{} to {}",
            start.format("%Y-%m-%d %H:%MZ"),
            end.format("%Y-%m-%d %H:%MZ")
        ),
        _ => "none".to_owned(),
    }
}

fn loop_speed_label(percent: u16) -> String {
    if percent == 100 {
        "1x".to_owned()
    } else if percent > 100 && percent.is_multiple_of(100) {
        format!("{}x", percent / 100)
    } else {
        format!("{:.2}x", f32::from(percent) / 100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_window_utc_parses_start_and_end_controls() {
        let state = UnifiedPlayerState {
            start_date_input: "2026-06-17".to_owned(),
            start_hour_input: "22".to_owned(),
            start_minute_input: "05".to_owned(),
            end_date_input: "2026-06-18".to_owned(),
            end_hour_input: "01".to_owned(),
            end_minute_input: "30".to_owned(),
            ..UnifiedPlayerState::default()
        };

        let (start, end) = state.archive_window_utc().expect("window parses");

        assert_eq!(start.to_rfc3339(), "2026-06-17T22:05:00+00:00");
        assert_eq!(end.to_rfc3339(), "2026-06-18T01:30:00+00:00");
    }

    #[test]
    fn warning_auto_sync_is_opt_in() {
        assert!(!UnifiedPlayerState::default().auto_sync_warnings);
        assert!(!UnifiedPlayerContext::default().auto_sync_warnings);
    }

    #[test]
    fn archive_window_utc_rejects_reversed_range() {
        let state = UnifiedPlayerState {
            start_date_input: "2026-06-18".to_owned(),
            start_hour_input: "02".to_owned(),
            start_minute_input: "00".to_owned(),
            end_date_input: "2026-06-18".to_owned(),
            end_hour_input: "01".to_owned(),
            end_minute_input: "00".to_owned(),
            ..UnifiedPlayerState::default()
        };

        assert_eq!(
            state.archive_window_utc().unwrap_err(),
            "End time must be after start time"
        );
    }
}
