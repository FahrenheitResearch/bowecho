//! Media sharing: screenshots to clipboard + disk, and loop recording.
//!
//! Capture flow (eframe 0.34): send
//! `egui::ViewportCommand::Screenshot(egui::UserData)` and the composited
//! window pixels come back as an `egui::Event::Screenshot` in a following
//! frame's raw input. The default capture is the FULL window so shared media
//! keeps the map plus any open sounding/WoFS/FARM windows as context.
//!
//! Loop recording steps the timeline deterministically: one selected
//! frame/cut step per captured screenshot, waiting for the async radar render
//! to settle before each capture, so the output contains exactly one clean
//! cycle regardless of wall-clock decode/render latency. Frames stream to a
//! background encoder thread (GIF via the `image` crate's NeuQuant
//! quantizer, or MP4/WebP by piping raw RGBA into an `ffmpeg` found on PATH).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::{LoopTimelineStep, LoopTimelineTarget, ViewerApp};

const CAPTURE_FILE_PREFIX: &str = "bowecho";
/// Extra repaints to wait after renders settle so the freshly uploaded
/// texture is actually painted before the capture frame.
const RECORD_SETTLE_FRAMES: u8 = 2;
/// Updates to wait for a frame's radar render before capturing anyway.
const RECORD_RENDER_TIMEOUT_FRAMES: u32 = 1_200;
/// Updates to wait for the screenshot event before aborting the recording.
const RECORD_CAPTURE_TIMEOUT_FRAMES: u32 = 600;
/// Free/manual recordings are meant for panning, zooming, and scrubbing the UI.
const DEFAULT_RECORD_FPS: u16 = 30;
const RECORD_FPS_CHOICES: [u16; 5] = [10, 15, 24, 30, 60];
const RECORD_FPS_MIN: u16 = 1;
const RECORD_FPS_MAX: u16 = 60;
pub(crate) const LOOP_RECORD_SPEED_PERCENT_OPTIONS: &[u16] =
    &[25, 50, 75, 100, 150, 200, 300, 400, 800, 1600, 3200, 6400];
/// GIF quantizer speed (1 = best/slowest, 30 = worst/fastest). The `image`
/// crate feeds this to NeuQuant (Dekker 1994, "Kohonen neural networks for
/// optimal colour quantization", Network: Computation in Neural Systems).
const GIF_QUANTIZER_SPEED: i32 = 10;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Tag attached to each screenshot request so replies can be routed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureKind {
    FullWindow,
    MapOnly,
    RecordFrame,
    FreeRecordFrame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordSize {
    Small720,
    Full1280,
    Hd1920,
    /// No downscale — frames keep the capture's full physical-pixel
    /// resolution (field request: higher-res exports). GIFs get big.
    Native,
}

impl RecordSize {
    fn label(self) -> &'static str {
        match self {
            Self::Small720 => "720",
            Self::Full1280 => "1280",
            Self::Hd1920 => "1920",
            Self::Native => "native",
        }
    }

    fn max_width(self) -> u32 {
        match self {
            Self::Small720 => 720,
            Self::Full1280 => 1280,
            Self::Hd1920 => 1920,
            // target_dimensions caps at min(width, max_width): MAX means
            // the capture resolution passes through untouched.
            Self::Native => u32::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordFormat {
    Auto,
    Gif,
    Mp4,
    WebP,
}

impl RecordFormat {
    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Gif => "GIF",
            Self::Mp4 => "MP4",
            Self::WebP => "WebP",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedRecordFormat {
    Gif,
    Mp4,
    WebP,
}

impl ResolvedRecordFormat {
    fn label(self) -> &'static str {
        match self {
            Self::Gif => "GIF",
            Self::Mp4 => "MP4",
            Self::WebP => "WebP",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Mp4 => "mp4",
            Self::WebP => "webp",
        }
    }
}

enum RecorderPhase {
    /// Select history frame `cursor` on the next drive.
    SelectFrame,
    /// Wait for async renders to settle, then a couple of paint frames.
    WaitRender { waited: u32, settle: u8 },
    /// Screenshot command sent; waiting for the capture event.
    AwaitScreenshot { waited: u32 },
}

struct RecorderState {
    target: LoopTimelineTarget,
    cursor: usize,
    total: usize,
    /// History length at record start; any change shifts indices, so abort.
    history_len: usize,
    steps: Vec<LoopTimelineStep>,
    phase: RecorderPhase,
    restore_index: usize,
    restore_cut: usize,
    restore_playing: bool,
    restore_browsing: bool,
    frame_tx: mpsc::Sender<EncoderMsg>,
    format: ResolvedRecordFormat,
}

enum FreeRecorderPhase {
    Ready,
    AwaitScreenshot { waited: u32 },
}

struct FreeRecorderState {
    frames: usize,
    phase: FreeRecorderPhase,
    next_capture_at: Instant,
    last_capture_at: Option<Instant>,
    frame_delay_ms: u32,
    frame_tx: mpsc::Sender<EncoderMsg>,
    format: ResolvedRecordFormat,
}

enum EncoderMsg {
    Frame {
        image: Arc<egui::ColorImage>,
        repeat: usize,
    },
    Finish,
    Abort,
}

pub(crate) struct MediaResult {
    message: String,
}

pub(crate) struct MediaShare {
    result_tx: mpsc::Sender<MediaResult>,
    result_rx: mpsc::Receiver<MediaResult>,
    /// Most recent map canvas rect (points), used for map-only crops.
    pub(crate) last_map_rect: Option<egui::Rect>,
    record_size: RecordSize,
    record_format: RecordFormat,
    record_fps: u16,
    /// Lazily detected on first record; `ffmpeg -version` on PATH.
    ffmpeg_available: Option<bool>,
    /// Lazily detected when WebP is requested; needs ffmpeg's animated WebP encoder.
    ffmpeg_webp_available: Option<bool>,
    recorder: Option<RecorderState>,
    free_recorder: Option<FreeRecorderState>,
}

impl Default for MediaShare {
    fn default() -> Self {
        let (result_tx, result_rx) = mpsc::channel();
        Self {
            result_tx,
            result_rx,
            last_map_rect: None,
            record_size: RecordSize::Small720,
            record_format: RecordFormat::Auto,
            record_fps: DEFAULT_RECORD_FPS,
            ffmpeg_available: None,
            ffmpeg_webp_available: None,
            recorder: None,
            free_recorder: None,
        }
    }
}

impl MediaShare {
    pub(crate) fn with_record_fps(record_fps: u16) -> Self {
        Self {
            record_fps: normalize_record_fps(record_fps),
            ..Self::default()
        }
    }

    pub(crate) fn is_recording(&self) -> bool {
        self.recorder.is_some() || self.free_recorder.is_some()
    }

    pub(crate) fn loop_recording(&self) -> bool {
        self.recorder.is_some()
    }

    pub(crate) fn free_recording(&self) -> bool {
        self.free_recorder.is_some()
    }

    pub(crate) fn record_settings_label(&self, loop_record_speed_percent: u16) -> String {
        format!(
            "{} {} · loop {} · free {}fps",
            self.record_size.label(),
            self.record_format.label(),
            loop_record_speed_label(loop_record_speed_percent),
            self.normalized_record_fps()
        )
    }

    fn normalized_record_fps(&self) -> u16 {
        normalize_record_fps(self.record_fps)
    }

    fn free_record_frame_delay_ms(&self) -> u32 {
        frame_delay_ms_for_fps(self.normalized_record_fps())
    }

    pub(crate) fn full_resolution_mp4_enabled(&self) -> bool {
        self.record_size == RecordSize::Native && self.record_format == RecordFormat::Mp4
    }

    pub(crate) fn use_full_resolution_mp4_preset(&mut self) {
        self.record_size = RecordSize::Native;
        self.record_format = RecordFormat::Mp4;
    }
}

impl ViewerApp {
    /// Per-frame media driver: polls background results, handles capture
    /// hotkeys, and routes returned screenshot events.
    pub(crate) fn handle_media(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.media.result_rx.try_recv() {
            self.status = result.message;
        }

        if ctx.input_mut(|input| {
            input.consume_key(
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                egui::Key::F12,
            )
        }) {
            self.toggle_recording(ctx, None);
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, egui::Key::F12)) {
            self.toggle_free_recording(ctx);
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, egui::Key::F12)) {
            self.request_screenshot(ctx, CaptureKind::MapOnly);
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::F12)) {
            self.request_screenshot(ctx, CaptureKind::FullWindow);
        }

        let captures: Vec<(CaptureKind, Arc<egui::ColorImage>)> = ctx.input(|input| {
            input
                .raw
                .events
                .iter()
                .filter_map(|event| {
                    let egui::Event::Screenshot {
                        user_data, image, ..
                    } = event
                    else {
                        return None;
                    };
                    let kind = user_data
                        .data
                        .as_ref()?
                        .downcast_ref::<CaptureKind>()
                        .copied()?;
                    Some((kind, Arc::clone(image)))
                })
                .collect()
        });
        for (kind, image) in captures {
            match kind {
                CaptureKind::FullWindow => self.finish_still_capture(ctx, &image, false),
                CaptureKind::MapOnly => self.finish_still_capture(ctx, &image, true),
                CaptureKind::RecordFrame => self.record_frame_captured(ctx, image),
                CaptureKind::FreeRecordFrame => self.free_record_frame_captured(ctx, image),
            }
        }

        self.drive_recorder(ctx);
        self.drive_free_recorder(ctx);
    }

    fn request_screenshot(&self, ctx: &egui::Context, kind: CaptureKind) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(kind)));
        ctx.request_repaint();
    }

    fn finish_still_capture(
        &mut self,
        ctx: &egui::Context,
        image: &egui::ColorImage,
        map_only: bool,
    ) {
        let image = if map_only && let Some(rect) = self.media.last_map_rect {
            crop_color_image(image, rect, ctx.pixels_per_point())
        } else {
            image.clone()
        };
        ctx.copy_image(image.clone());

        let result_tx = self.media.result_tx.clone();
        let repaint_ctx = ctx.clone();
        thread::spawn(move || {
            let message = match save_capture_png(&image) {
                Ok(path) => format!("Screenshot copied to clipboard + saved {}", path.display()),
                Err(err) => format!("Screenshot copied to clipboard; PNG save failed: {err}"),
            };
            let _ = result_tx.send(MediaResult { message });
            repaint_ctx.request_repaint();
        });
        self.status = "Screenshot copied to clipboard; saving PNG...".to_owned();
    }

    pub(crate) fn media_top_bar_ui(&mut self, ui: &mut egui::Ui) {
        if crate::fixed_action_button(ui, "Screenshot", 86.0)
            .on_hover_text(
                "Copy a full-window screenshot to the clipboard and save a PNG \
                 (F12; Shift+F12 captures the map only)",
            )
            .clicked()
        {
            self.request_screenshot(ui.ctx(), CaptureKind::FullWindow);
        }

        if let Some(recorder) = &self.media.recorder {
            // Hide the badge while a capture is in flight so REC does not get
            // burned into the recorded frames themselves.
            let show = !matches!(recorder.phase, RecorderPhase::AwaitScreenshot { .. });
            ui.add_visible(
                show,
                egui::Label::new(
                    egui::RichText::new(format!(
                        "REC {}/{}",
                        (recorder.cursor + 1).min(recorder.total),
                        recorder.total
                    ))
                    .color(egui::Color32::from_rgb(235, 64, 52))
                    .strong(),
                ),
            );
        } else if let Some(recorder) = &self.media.free_recorder {
            let show = !matches!(recorder.phase, FreeRecorderPhase::AwaitScreenshot { .. });
            ui.add_visible(
                show,
                egui::Label::new(
                    egui::RichText::new(format!("REC {}", recorder.frames))
                        .color(egui::Color32::from_rgb(235, 64, 52))
                        .strong(),
                ),
            );
        }
    }

    /// Record button + output options, drawn next to the playback controls.
    pub(crate) fn record_controls_ui_for_target(
        &mut self,
        ui: &mut egui::Ui,
        target: Option<LoopTimelineTarget>,
    ) {
        let loop_recording = self.media.recorder.is_some();
        let free_recording = self.media.free_recorder.is_some();
        let record_label = if loop_recording { "Stop" } else { "Loop" };
        let step_count = target
            .map(|target| self.loop_timeline_steps_for_target(target).len())
            .unwrap_or_else(|| self.loop_timeline_steps_for_recording().len());
        let record_enabled = !free_recording && (loop_recording || step_count > 1);
        if ui
            .add_enabled_ui(record_enabled, |ui| {
                crate::fixed_action_button(ui, record_label, 62.0)
            })
            .inner
            .on_hover_text(
                "Record one clean cycle of this loop to a shareable GIF/MP4 in \
                 Pictures/BowEcho (needs 2+ frames; pan/zoom during recording \
                 is captured too). Hotkey: Ctrl+Shift+F12",
            )
            .clicked()
        {
            self.toggle_recording(ui.ctx(), target);
        }

        let free_label = if free_recording { "Stop" } else { "Free" };
        if ui
            .add_enabled_ui(!loop_recording, |ui| {
                crate::fixed_action_button(ui, free_label, 54.0)
            })
            .inner
            .on_hover_text(
                "Start/stop a free full-window recording of your app movement. \
                 Pan, zoom, scrub, change panes, then stop. Hotkey: Ctrl+F12",
            )
            .clicked()
        {
            self.toggle_free_recording(ui.ctx());
        }

        self.record_options_ui(ui);
    }

    pub(crate) fn record_options_ui(&mut self, ui: &mut egui::Ui) {
        let loop_recording = self.media.recorder.is_some();
        let free_recording = self.media.free_recorder.is_some();
        ui.add_enabled_ui(!loop_recording && !free_recording, |ui| {
            egui::ComboBox::from_id_salt("media_record_size")
                .selected_text(self.media.record_size.label())
                .width(56.0)
                .show_ui(ui, |ui| {
                    for size in [
                        RecordSize::Small720,
                        RecordSize::Full1280,
                        RecordSize::Hd1920,
                        RecordSize::Native,
                    ] {
                        ui.selectable_value(&mut self.media.record_size, size, size.label());
                    }
                })
                .response
                .on_hover_text(
                    "Maximum recording width in pixels (smaller = Discord-friendlier). \
                     native = the full capture resolution, no downscale — crispest, \
                     biggest files (MP4 handles it well; GIFs get large)",
                );
            egui::ComboBox::from_id_salt("media_record_format")
                .selected_text(self.media.record_format.label())
                .width(56.0)
                .show_ui(ui, |ui| {
                    for format in [
                        RecordFormat::Auto,
                        RecordFormat::Gif,
                        RecordFormat::Mp4,
                        RecordFormat::WebP,
                    ] {
                        ui.selectable_value(&mut self.media.record_format, format, format.label());
                    }
                })
                .response
                .on_hover_text(
                    "Auto = MP4 when ffmpeg is on PATH, otherwise GIF. WebP uses ffmpeg's animated WebP encoder.",
                );
            let before_loop_speed = self.app_settings.loop_record_speed_percent;
            let mut loop_speed =
                normalize_loop_record_speed_percent(self.app_settings.loop_record_speed_percent);
            egui::ComboBox::from_id_salt("media_loop_record_speed")
                .selected_text(format!("loop {}", loop_record_speed_label(loop_speed)))
                .width(76.0)
                .show_ui(ui, |ui| {
                    for speed in LOOP_RECORD_SPEED_PERCENT_OPTIONS {
                        ui.selectable_value(
                            &mut loop_speed,
                            *speed,
                            loop_record_speed_label(*speed),
                        );
                    }
                })
                .response
                .on_hover_text(
                    "GIF/MP4/WebP loop export speed. Separate from screen playback so giant low-sweep loops can be compressed intentionally.",
                );
            self.app_settings.loop_record_speed_percent = loop_speed;
            if self.app_settings.loop_record_speed_percent != before_loop_speed {
                let _ = self.app_settings.save();
            }
            let before_fps = self.media.record_fps;
            egui::ComboBox::from_id_salt("media_record_fps")
                .selected_text(format!("{}fps", self.media.normalized_record_fps()))
                .width(62.0)
                .show_ui(ui, |ui| {
                    for fps in RECORD_FPS_CHOICES {
                        ui.selectable_value(&mut self.media.record_fps, fps, format!("{fps}fps"));
                    }
                })
                .response
                .on_hover_text(
                    "Free recording capture/playback FPS. Loop recording exports at a stable 1x radar cadence so high-speed playback does not make one-second loops.",
                );
            self.media.record_fps = normalize_record_fps(self.media.record_fps);
            if self.media.record_fps != before_fps {
                self.app_settings.record_fps = self.media.record_fps;
                let _ = self.app_settings.save();
            }
        });
    }

    pub(crate) fn loop_record_renders_settled(&self) -> bool {
        self.pending_render_key.is_none()
            && self
                .extra_panes
                .iter()
                .all(|pane| pane.pending_render_key.is_none())
            && self
                .radar_layers
                .iter()
                .all(|layer| layer.pending_render_key.is_none())
    }

    pub(crate) fn loop_record_selected_textures_ready(&self, target: LoopTimelineTarget) -> bool {
        match target {
            LoopTimelineTarget::Primary => {
                self.loop_record_primary_texture_ready()
                    && self.loop_record_radar_layers_textures_ready()
                    && self
                        .extra_panes
                        .iter()
                        .take(self.grid_layout.panel_count().saturating_sub(1))
                        .enumerate()
                        .all(|(slot, pane)| {
                            !self.extra_pane_participates_in_primary_timeline(pane)
                                || self.loop_record_extra_pane_texture_ready(slot)
                        })
            }
            LoopTimelineTarget::ExtraPane(slot) => {
                self.loop_record_extra_pane_texture_ready(slot)
                    && self.loop_record_radar_layers_textures_ready()
            }
        }
    }

    fn loop_record_primary_texture_ready(&self) -> bool {
        let Some(volume) = self.volume.as_ref() else {
            return true;
        };
        if self.selected_cut >= volume.cuts.len() {
            return true;
        }
        let render_volume =
            self.display_source_volume_for_product(&self.selected_product, Arc::clone(volume));
        self.texture_key.as_ref().is_some_and(|key| {
            key.volume_ptr == Arc::as_ptr(&render_volume) as usize
                && key.cut == self.selected_cut
                && key.product == self.selected_product
        })
    }

    fn loop_record_extra_pane_texture_ready(&self, pane_slot: usize) -> bool {
        let Some(pane) = self.extra_panes.get(pane_slot) else {
            return true;
        };
        let Some(volume) = self.extra_pane_display_volume(pane_slot) else {
            return true;
        };
        let preferred_cut = pane.cut.unwrap_or(self.selected_cut);
        let Some(cut) =
            crate::display_cut_for_product(volume.as_ref(), preferred_cut, &pane.product)
        else {
            return true;
        };
        pane.texture_key.as_ref().is_some_and(|key| {
            key.volume_ptr == Arc::as_ptr(&volume) as usize
                && key.cut == cut
                && key.product == pane.product
        })
    }

    fn loop_record_radar_layers_textures_ready(&self) -> bool {
        self.radar_layers
            .iter()
            .filter(|layer| layer.visible)
            .all(|layer| {
                let Some(volume) = layer.volume.as_ref() else {
                    return true;
                };
                let product = self.selected_product.clone();
                let Some(cut) =
                    crate::display_cut_for_product(volume.as_ref(), self.selected_cut, &product)
                else {
                    return true;
                };
                layer.texture_key.as_ref().is_some_and(|key| {
                    key.volume_ptr == Arc::as_ptr(volume) as usize
                        && key.cut == cut
                        && key.product == product
                })
            })
    }

    pub(crate) fn loop_record_timeline_overlays_settled(&self) -> bool {
        if !self.hazards_visible
            || self.loop_timeline_history_len(self.active_loop_timeline_target()) <= 1
        {
            return true;
        }
        if self.hazard_receiver.is_some()
            && (self.event_loop_hazard_window.is_some() || self.unified_player.auto_sync_warnings)
        {
            return false;
        }
        if self.unified_player.auto_sync_warnings {
            return self.timeline_warning_window_covers_loaded_loop();
        }
        true
    }

    pub(crate) fn loop_record_time_layers_settled(&self) -> bool {
        if self.loop_timeline_steps_for_recording().len() <= 1 {
            return true;
        }
        if !self.timeline_satellite_map_settled() {
            return false;
        }
        self.timeline_model_layers_settled()
    }

    fn toggle_recording(&mut self, ctx: &egui::Context, target: Option<LoopTimelineTarget>) {
        if self.media.recorder.is_some() {
            self.finish_recording(ctx);
        } else {
            self.start_recording(ctx, target);
        }
    }

    pub(crate) fn toggle_loop_recording_from_unified_player(&mut self, ctx: &egui::Context) {
        self.toggle_recording(ctx, None);
    }

    fn start_recording(&mut self, ctx: &egui::Context, target: Option<LoopTimelineTarget>) {
        if self.media.free_recorder.is_some() {
            self.status = "Stop free recording before starting loop recording".to_owned();
            return;
        }
        let target = target.unwrap_or_else(|| self.active_loop_timeline_target());
        let steps = self.loop_timeline_steps_for_target(target);
        let total = steps.len();
        if total < 2 {
            self.status =
                "Recording needs at least 2 history frames (use Load Loop first)".to_owned();
            return;
        }
        let (restore_index, restore_cut, restore_playing, restore_browsing) = match target {
            LoopTimelineTarget::Primary => (
                self.selected_frame_index,
                self.selected_cut,
                self.history_playing,
                self.browsing_history,
            ),
            LoopTimelineTarget::ExtraPane(slot) => self
                .extra_panes
                .get(slot)
                .map(|pane| {
                    (
                        pane.selected_frame_index,
                        pane.cut.unwrap_or(self.selected_cut),
                        pane.history_playing,
                        pane.browsing_history,
                    )
                })
                .unwrap_or((
                    self.selected_frame_index,
                    self.selected_cut,
                    self.history_playing,
                    self.browsing_history,
                )),
        };
        self.maybe_auto_sync_timeline_warnings(ctx);
        let waiting_for_timeline_overlays = !self.loop_record_timeline_overlays_settled();
        let waiting_for_time_layers = !self.loop_record_time_layers_settled();
        let (format, fallback_note) = self.resolve_record_format();
        let path = match new_capture_path(format.extension()) {
            Ok(path) => path,
            Err(err) => {
                self.status = format!("Recording failed: {err}");
                return;
            }
        };
        let frame_tx = spawn_loop_encoder(LoopEncodeJob {
            format,
            max_width: self.media.record_size.max_width(),
            // Deterministic loop exports use the stable 1x radar cadence,
            // not whatever high-speed playback rate the operator is using
            // to scrub around on screen.
            frame_delay_ms: self.loop_record_frame_ms() as u32,
            frame_rate_fps: None,
            out_path: path,
            result_tx: self.media.result_tx.clone(),
            repaint_ctx: ctx.clone(),
        });
        self.media.recorder = Some(RecorderState {
            target,
            cursor: 0,
            total,
            history_len: self.loop_timeline_history_len(target),
            steps,
            phase: RecorderPhase::SelectFrame,
            restore_index,
            restore_cut,
            restore_playing,
            restore_browsing,
            frame_tx,
            format,
        });
        match target {
            LoopTimelineTarget::Primary => {
                self.history_playing = false;
                // Latch browsing so an in-flight live load cannot steal the
                // selection mid-recording.
                self.browsing_history = true;
            }
            LoopTimelineTarget::ExtraPane(slot) => {
                if let Some(pane) = self.extra_panes.get_mut(slot) {
                    pane.history_playing = false;
                    pane.browsing_history = true;
                }
            }
        }
        self.status = if let Some(fallback_note) = fallback_note {
            format!("{fallback_note}; recording {total} frames as GIF instead...")
        } else if waiting_for_time_layers {
            format!(
                "Recording loop: waiting for timeline layers, then {total} frames as {}...",
                format.label()
            )
        } else if waiting_for_timeline_overlays {
            format!(
                "Recording loop: waiting for timeline warnings, then {total} frames as {}...",
                format.label()
            )
        } else {
            format!("Recording loop: {total} frames as {}...", format.label())
        };
        ctx.request_repaint();
    }

    fn toggle_free_recording(&mut self, ctx: &egui::Context) {
        if self.media.free_recorder.is_some() {
            self.finish_free_recording(ctx);
        } else {
            self.start_free_recording(ctx);
        }
    }

    pub(crate) fn toggle_free_recording_from_unified_player(&mut self, ctx: &egui::Context) {
        self.toggle_free_recording(ctx);
    }

    fn start_free_recording(&mut self, ctx: &egui::Context) {
        if self.media.recorder.is_some() {
            self.status = "Stop loop recording before starting free recording".to_owned();
            return;
        }
        let (format, fallback_note) = self.resolve_record_format();
        let path = match new_capture_path(format.extension()) {
            Ok(path) => path,
            Err(err) => {
                self.status = format!("Free recording failed: {err}");
                return;
            }
        };
        let record_fps = self.media.normalized_record_fps();
        let frame_delay_ms = self.media.free_record_frame_delay_ms();
        let frame_tx = spawn_loop_encoder(LoopEncodeJob {
            format,
            max_width: self.media.record_size.max_width(),
            frame_delay_ms,
            frame_rate_fps: Some(record_fps),
            out_path: path,
            result_tx: self.media.result_tx.clone(),
            repaint_ctx: ctx.clone(),
        });
        self.media.free_recorder = Some(FreeRecorderState {
            frames: 0,
            phase: FreeRecorderPhase::Ready,
            next_capture_at: Instant::now(),
            last_capture_at: None,
            frame_delay_ms,
            frame_tx,
            format,
        });
        self.status = if let Some(fallback_note) = fallback_note {
            format!("{fallback_note}; free recording as GIF instead...")
        } else {
            format!(
                "Free recording as {} at {record_fps}fps... Ctrl+F12 to stop",
                format.label()
            )
        };
        ctx.request_repaint();
    }

    fn resolve_record_format(&mut self) -> (ResolvedRecordFormat, Option<&'static str>) {
        let ffmpeg = *self
            .media
            .ffmpeg_available
            .get_or_insert_with(detect_ffmpeg);
        let mut webp_available = || {
            *self
                .media
                .ffmpeg_webp_available
                .get_or_insert_with(detect_ffmpeg_webp_anim)
        };
        match self.media.record_format {
            RecordFormat::Gif => (ResolvedRecordFormat::Gif, None),
            RecordFormat::Mp4 if ffmpeg => (ResolvedRecordFormat::Mp4, None),
            RecordFormat::Mp4 => (ResolvedRecordFormat::Gif, Some("ffmpeg not found on PATH")),
            RecordFormat::WebP if !ffmpeg => {
                (ResolvedRecordFormat::Gif, Some("ffmpeg not found on PATH"))
            }
            RecordFormat::WebP if webp_available() => (ResolvedRecordFormat::WebP, None),
            RecordFormat::WebP => (
                ResolvedRecordFormat::Gif,
                Some("ffmpeg animated WebP encoder not found"),
            ),
            RecordFormat::Auto if ffmpeg => (ResolvedRecordFormat::Mp4, None),
            RecordFormat::Auto => (ResolvedRecordFormat::Gif, None),
        }
    }

    /// Advances the deterministic record state machine by one update.
    fn drive_recorder(&mut self, ctx: &egui::Context) {
        if self.media.recorder.is_none() {
            return;
        }
        if self.media.recorder.as_ref().is_some_and(|recorder| {
            recorder.history_len != self.loop_timeline_history_len(recorder.target)
        }) {
            self.abort_recording(ctx, "history changed during recording");
            return;
        }

        let renders_settled = self.loop_record_renders_settled();
        let selected_textures_ready = self
            .media
            .recorder
            .as_ref()
            .is_none_or(|recorder| self.loop_record_selected_textures_ready(recorder.target));
        let timeline_overlays_settled = self.loop_record_timeline_overlays_settled();
        let time_layers_settled = self.loop_record_time_layers_settled();

        enum DriveAction {
            None,
            Select(LoopTimelineTarget, LoopTimelineStep),
            Capture,
            AbortCaptureTimeout,
        }
        let action = {
            let Some(recorder) = self.media.recorder.as_mut() else {
                return;
            };
            match &mut recorder.phase {
                RecorderPhase::SelectFrame => {
                    let step = recorder.steps.get(recorder.cursor).copied();
                    let target = recorder.target;
                    recorder.phase = RecorderPhase::WaitRender {
                        waited: 0,
                        settle: RECORD_SETTLE_FRAMES,
                    };
                    match step {
                        Some(step) => DriveAction::Select(target, step),
                        None => DriveAction::AbortCaptureTimeout,
                    }
                }
                RecorderPhase::WaitRender { waited, settle } => {
                    if (renders_settled
                        && selected_textures_ready
                        && timeline_overlays_settled
                        && time_layers_settled)
                        || *waited > RECORD_RENDER_TIMEOUT_FRAMES
                    {
                        if *settle == 0 {
                            recorder.phase = RecorderPhase::AwaitScreenshot { waited: 0 };
                            DriveAction::Capture
                        } else {
                            *settle -= 1;
                            DriveAction::None
                        }
                    } else {
                        *waited += 1;
                        DriveAction::None
                    }
                }
                RecorderPhase::AwaitScreenshot { waited } => {
                    *waited += 1;
                    if *waited > RECORD_CAPTURE_TIMEOUT_FRAMES {
                        DriveAction::AbortCaptureTimeout
                    } else {
                        DriveAction::None
                    }
                }
            }
        };
        match action {
            DriveAction::None => {}
            DriveAction::Select(target, step) => self.select_history_record_step(target, step, ctx),
            DriveAction::Capture => self.request_screenshot(ctx, CaptureKind::RecordFrame),
            DriveAction::AbortCaptureTimeout => {
                self.abort_recording(ctx, "screenshot capture timed out");
                return;
            }
        }
        ctx.request_repaint();
    }

    fn record_frame_captured(&mut self, ctx: &egui::Context, image: Arc<egui::ColorImage>) {
        let Some(recorder) = self.media.recorder.as_mut() else {
            return;
        };
        let _ = recorder
            .frame_tx
            .send(EncoderMsg::Frame { image, repeat: 1 });
        recorder.cursor += 1;
        if recorder.cursor >= recorder.total {
            self.finish_recording(ctx);
        } else {
            recorder.phase = RecorderPhase::SelectFrame;
            ctx.request_repaint();
        }
    }

    /// Drives the free/manual recorder at the selected viewport-recording FPS
    /// until the user stops it.
    fn drive_free_recorder(&mut self, ctx: &egui::Context) {
        let Some(recorder) = self.media.free_recorder.as_mut() else {
            return;
        };
        enum DriveAction {
            None,
            Capture,
            AbortCaptureTimeout,
        }
        let now = Instant::now();
        let action = match &mut recorder.phase {
            FreeRecorderPhase::Ready => {
                if now >= recorder.next_capture_at {
                    recorder.phase = FreeRecorderPhase::AwaitScreenshot { waited: 0 };
                    DriveAction::Capture
                } else {
                    ctx.request_repaint_after(recorder.next_capture_at - now);
                    DriveAction::None
                }
            }
            FreeRecorderPhase::AwaitScreenshot { waited } => {
                *waited += 1;
                if *waited > RECORD_CAPTURE_TIMEOUT_FRAMES {
                    DriveAction::AbortCaptureTimeout
                } else {
                    DriveAction::None
                }
            }
        };
        match action {
            DriveAction::None => {}
            DriveAction::Capture => self.request_screenshot(ctx, CaptureKind::FreeRecordFrame),
            DriveAction::AbortCaptureTimeout => {
                self.abort_free_recording(ctx, "screenshot capture timed out");
                return;
            }
        }
        ctx.request_repaint();
    }

    fn free_record_frame_captured(&mut self, ctx: &egui::Context, image: Arc<egui::ColorImage>) {
        let Some(recorder) = self.media.free_recorder.as_mut() else {
            return;
        };
        let now = Instant::now();
        let repeat = recorder
            .last_capture_at
            .map(|last| {
                frame_repeat_for_elapsed(
                    now.saturating_duration_since(last),
                    recorder.frame_delay_ms,
                )
            })
            .unwrap_or(1);
        recorder.last_capture_at = Some(now);
        let _ = recorder.frame_tx.send(EncoderMsg::Frame { image, repeat });
        recorder.frames += 1;
        recorder.phase = FreeRecorderPhase::Ready;
        let frame_delay = Duration::from_millis(u64::from(recorder.frame_delay_ms));
        recorder.next_capture_at += frame_delay;
        if recorder.next_capture_at <= now {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(recorder.next_capture_at - now);
        }
    }

    /// Ends the recording (loop complete or user pressed Stop) and hands the
    /// captured frames to the background encoder.
    fn finish_recording(&mut self, ctx: &egui::Context) {
        let Some(recorder) = self.media.recorder.take() else {
            return;
        };
        let status = if recorder.cursor == 0 {
            let _ = recorder.frame_tx.send(EncoderMsg::Abort);
            "Recording cancelled (no frames captured)".to_owned()
        } else {
            let _ = recorder.frame_tx.send(EncoderMsg::Finish);
            format!(
                "Encoding {} ({} frames) in the background...",
                recorder.format.label(),
                recorder.cursor
            )
        };
        self.restore_after_recording(&recorder, ctx);
        self.status = status;
        ctx.request_repaint();
    }

    fn abort_recording(&mut self, ctx: &egui::Context, reason: &str) {
        let Some(recorder) = self.media.recorder.take() else {
            return;
        };
        let _ = recorder.frame_tx.send(EncoderMsg::Abort);
        self.restore_after_recording(&recorder, ctx);
        self.status = format!("Recording aborted: {reason}");
        ctx.request_repaint();
    }

    fn finish_free_recording(&mut self, ctx: &egui::Context) {
        let Some(recorder) = self.media.free_recorder.take() else {
            return;
        };
        self.status = if recorder.frames == 0 {
            let _ = recorder.frame_tx.send(EncoderMsg::Abort);
            "Free recording cancelled (no frames captured)".to_owned()
        } else {
            let _ = recorder.frame_tx.send(EncoderMsg::Finish);
            format!(
                "Encoding {} free recording ({} frames) in the background...",
                recorder.format.label(),
                recorder.frames
            )
        };
        ctx.request_repaint();
    }

    fn abort_free_recording(&mut self, ctx: &egui::Context, reason: &str) {
        let Some(recorder) = self.media.free_recorder.take() else {
            return;
        };
        let _ = recorder.frame_tx.send(EncoderMsg::Abort);
        self.status = format!("Free recording aborted: {reason}");
        ctx.request_repaint();
    }

    fn restore_after_recording(&mut self, recorder: &RecorderState, ctx: &egui::Context) {
        if let LoopTimelineTarget::ExtraPane(slot) = recorder.target {
            let restore_index = self
                .extra_panes
                .get(slot)
                .map(|pane| {
                    recorder
                        .restore_index
                        .min(pane.frame_history.len().saturating_sub(1))
                })
                .unwrap_or(0);
            if self
                .extra_panes
                .get(slot)
                .is_some_and(|pane| !pane.frame_history.is_empty())
            {
                self.select_extra_pane_history_frame(slot, restore_index, ctx);
                let can_restore_playing = self.extra_pane_history_loop_can_step(slot);
                if let Some(pane) = self.extra_panes.get_mut(slot) {
                    if pane
                        .volume
                        .as_ref()
                        .is_some_and(|volume| recorder.restore_cut < volume.cuts.len())
                    {
                        Self::set_pane_cut_preserving_texture(pane, recorder.restore_cut);
                    }
                    pane.history_playing = recorder.restore_playing && can_restore_playing;
                    pane.browsing_history = if pane.history_playing {
                        false
                    } else {
                        recorder.restore_browsing
                    };
                }
            }
            return;
        }
        let restore_index = recorder
            .restore_index
            .min(self.frame_history.len().saturating_sub(1));
        if !self.frame_history.is_empty() {
            self.select_history_frame(restore_index, false, ctx);
            if self
                .volume
                .as_ref()
                .is_some_and(|volume| recorder.restore_cut < volume.cuts.len())
            {
                self.set_selected_cut_preserving_texture(recorder.restore_cut);
                self.sync_following_extra_panes_to_current_low_sweep_rank(ctx);
            }
        }
        self.history_playing = recorder.restore_playing && self.primary_history_loop_can_step();
        self.browsing_history = if self.history_playing {
            false
        } else {
            recorder.restore_browsing
        };
    }

    pub(crate) fn select_history_record_step(
        &mut self,
        target: LoopTimelineTarget,
        step: LoopTimelineStep,
        ctx: &egui::Context,
    ) {
        if let LoopTimelineTarget::ExtraPane(slot) = target {
            self.select_extra_pane_timeline_step(slot, step, ctx);
            return;
        }
        self.select_history_frame_with_options(step.frame_index, false, false, ctx);
        let Some(cut_index) = step.cut_index else {
            return;
        };
        if step.low_sweep_rank.is_some() {
            self.set_shared_low_sweep_step(step, ctx);
            self.sync_active_timeline_side_effects(ctx);
            if let Some(label) = self.apply_camera_follow_for_current_frame(ctx) {
                let frame_status = self.selected_frame_status_text();
                self.status = format!("{frame_status} - {label}");
            }
            ctx.request_repaint();
            return;
        }
        self.sync_owned_extra_panes_to_primary_timeline_frame(step.frame_index, ctx);
        if self
            .volume
            .as_ref()
            .is_none_or(|volume| cut_index >= volume.cuts.len())
        {
            return;
        }
        self.set_selected_cut_preserving_texture(cut_index);
        self.sync_following_extra_panes_to_current_low_sweep_rank(ctx);
        self.sync_active_timeline_side_effects(ctx);
        if let Some(label) = self.apply_camera_follow_for_current_frame(ctx) {
            let frame_status = self.selected_frame_status_text();
            self.status = format!("{frame_status} - {label}");
        }
        ctx.request_repaint();
    }
}

struct LoopEncodeJob {
    format: ResolvedRecordFormat,
    max_width: u32,
    frame_delay_ms: u32,
    frame_rate_fps: Option<u16>,
    out_path: PathBuf,
    result_tx: mpsc::Sender<MediaResult>,
    repaint_ctx: egui::Context,
}

/// Spawns the background encoder thread and returns its frame channel.
/// Frames stream in as they are captured so memory stays bounded.
fn spawn_loop_encoder(job: LoopEncodeJob) -> mpsc::Sender<EncoderMsg> {
    let (frame_tx, frame_rx) = mpsc::channel::<EncoderMsg>();
    thread::spawn(move || {
        let message = run_loop_encoder(&job, &frame_rx);
        if let Some(message) = message {
            let _ = job.result_tx.send(MediaResult { message });
            job.repaint_ctx.request_repaint();
        }
    });
    frame_tx
}

/// Returns the status message to surface, or `None` for a silent abort.
fn run_loop_encoder(job: &LoopEncodeJob, frame_rx: &mpsc::Receiver<EncoderMsg>) -> Option<String> {
    let mut sink = match LoopSink::new(job) {
        Ok(sink) => sink,
        Err(err) => {
            drain_until_end(frame_rx);
            return Some(format!("Recording failed: {err}"));
        }
    };
    let mut frames = 0_usize;
    loop {
        match frame_rx.recv() {
            Ok(EncoderMsg::Frame { image, repeat }) => {
                let repeat = repeat.max(1);
                for _ in 0..repeat {
                    if let Err(err) = sink.push(&image) {
                        drain_until_end(frame_rx);
                        sink.discard(&job.out_path);
                        return Some(format!("Recording failed: {err}"));
                    }
                }
                frames += repeat;
            }
            Ok(EncoderMsg::Finish) => {
                return Some(match sink.finish(&job.out_path) {
                    Ok(()) => {
                        let copy_note = match copy_recording_file_to_clipboard(&job.out_path) {
                            Ok(()) => " and copied the file to clipboard".to_owned(),
                            Err(err) => format!(" (file clipboard copy failed: {err})"),
                        };
                        format!(
                            "Saved {} recording ({frames} frames){copy_note}: {}",
                            job.format.label(),
                            job.out_path.display()
                        )
                    }
                    Err(err) => format!("Recording failed: {err}"),
                });
            }
            Ok(EncoderMsg::Abort) | Err(_) => {
                sink.discard(&job.out_path);
                return None;
            }
        }
    }
}

fn drain_until_end(frame_rx: &mpsc::Receiver<EncoderMsg>) {
    while let Ok(message) = frame_rx.recv() {
        if matches!(message, EncoderMsg::Finish | EncoderMsg::Abort) {
            return;
        }
    }
}

enum LoopSink {
    Gif(GifSink),
    Mp4(FfmpegSink),
    WebP(FfmpegSink),
}

impl LoopSink {
    fn new(job: &LoopEncodeJob) -> Result<Self, String> {
        match job.format {
            ResolvedRecordFormat::Gif => Ok(Self::Gif(GifSink::create(
                &job.out_path,
                job.max_width,
                job.frame_delay_ms,
            )?)),
            ResolvedRecordFormat::Mp4 => Ok(Self::Mp4(FfmpegSink::new(
                job.out_path.clone(),
                job.max_width,
                job.frame_delay_ms,
                job.frame_rate_fps,
                job.format,
            ))),
            ResolvedRecordFormat::WebP => Ok(Self::WebP(FfmpegSink::new(
                job.out_path.clone(),
                job.max_width,
                job.frame_delay_ms,
                job.frame_rate_fps,
                job.format,
            ))),
        }
    }

    fn push(&mut self, image: &egui::ColorImage) -> Result<(), String> {
        match self {
            Self::Gif(sink) => sink.push(image),
            Self::Mp4(sink) => sink.push(image),
            Self::WebP(sink) => sink.push(image),
        }
    }

    fn finish(self, out_path: &Path) -> Result<(), String> {
        match self {
            Self::Gif(sink) => sink.finish(),
            Self::Mp4(sink) => sink.finish(out_path),
            Self::WebP(sink) => sink.finish(out_path),
        }
    }

    fn discard(self, out_path: &Path) {
        match self {
            Self::Gif(sink) => drop(sink),
            Self::Mp4(sink) => sink.discard(),
            Self::WebP(sink) => sink.discard(),
        }
        let _ = std::fs::remove_file(out_path);
    }
}

/// Streaming animated-GIF writer. All frames are scaled to the dimensions
/// locked in by the first frame so mid-recording window resizes cannot
/// corrupt the stream.
struct GifSink {
    encoder: image::codecs::gif::GifEncoder<std::io::BufWriter<std::fs::File>>,
    max_width: u32,
    frame_delay_ms: u32,
    locked_dims: Option<(u32, u32)>,
}

impl GifSink {
    fn create(path: &Path, max_width: u32, frame_delay_ms: u32) -> Result<Self, String> {
        let file = std::fs::File::create(path)
            .map_err(|err| format!("could not create {}: {err}", path.display()))?;
        let mut encoder = image::codecs::gif::GifEncoder::new_with_speed(
            std::io::BufWriter::new(file),
            GIF_QUANTIZER_SPEED,
        );
        encoder
            .set_repeat(image::codecs::gif::Repeat::Infinite)
            .map_err(|err| format!("gif repeat: {err}"))?;
        Ok(Self {
            encoder,
            max_width,
            frame_delay_ms,
            locked_dims: None,
        })
    }

    fn push(&mut self, image: &egui::ColorImage) -> Result<(), String> {
        let frame = scaled_record_frame(image, self.max_width, &mut self.locked_dims)?;
        let delay = image::Delay::from_numer_denom_ms(self.frame_delay_ms, 1);
        self.encoder
            .encode_frame(image::Frame::from_parts(frame, 0, 0, delay))
            .map_err(|err| format!("gif encode: {err}"))
    }

    fn finish(self) -> Result<(), String> {
        // Dropping the encoder flushes the trailer.
        Ok(())
    }
}

/// Pipes raw RGBA frames into `ffmpeg` for MP4/WebP output. The child is
/// spawned lazily on the first frame (dimensions must be known up front).
struct FfmpegSink {
    out_path: PathBuf,
    max_width: u32,
    frame_delay_ms: u32,
    frame_rate_fps: Option<u16>,
    format: ResolvedRecordFormat,
    locked_dims: Option<(u32, u32)>,
    child: Option<Child>,
}

impl FfmpegSink {
    fn new(
        out_path: PathBuf,
        max_width: u32,
        frame_delay_ms: u32,
        frame_rate_fps: Option<u16>,
        format: ResolvedRecordFormat,
    ) -> Self {
        Self {
            out_path,
            max_width,
            frame_delay_ms,
            frame_rate_fps,
            format,
            locked_dims: None,
            child: None,
        }
    }

    fn push(&mut self, image: &egui::ColorImage) -> Result<(), String> {
        let frame = scaled_record_frame(image, self.max_width, &mut self.locked_dims)?;
        if self.child.is_none() {
            let (width, height) = frame.dimensions();
            let args = ffmpeg_encode_args(
                width,
                height,
                self.frame_delay_ms,
                self.frame_rate_fps,
                self.format,
                &self.out_path,
            );
            let mut command = Command::new("ffmpeg");
            command
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            hide_console_window(&mut command);
            self.child = Some(
                command
                    .spawn()
                    .map_err(|err| format!("ffmpeg spawn: {err}"))?,
            );
        }
        let stdin = self
            .child
            .as_mut()
            .and_then(|child| child.stdin.as_mut())
            .ok_or_else(|| "ffmpeg stdin unavailable".to_owned())?;
        stdin
            .write_all(frame.as_raw())
            .map_err(|err| format!("ffmpeg pipe: {err}"))
    }

    fn finish(mut self, out_path: &Path) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            return Err("no frames were piped to ffmpeg".to_owned());
        };
        drop(child.stdin.take());
        let status = child.wait().map_err(|err| format!("ffmpeg wait: {err}"))?;
        if status.success() {
            Ok(())
        } else {
            let _ = std::fs::remove_file(out_path);
            Err(format!("ffmpeg exited with {status}"))
        }
    }

    fn discard(mut self) {
        if let Some(mut child) = self.child.take() {
            drop(child.stdin.take());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Converts a captured frame to RGBA and scales it to the recording size.
/// The first frame locks the output dimensions (kept even for yuv420p).
fn scaled_record_frame(
    image: &egui::ColorImage,
    max_width: u32,
    locked_dims: &mut Option<(u32, u32)>,
) -> Result<image::RgbaImage, String> {
    let width = image.size[0] as u32;
    let height = image.size[1] as u32;
    if width == 0 || height == 0 {
        return Err("captured frame was empty".to_owned());
    }
    let rgba = rgba_bytes_from_color_image(image);
    let frame = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| "captured frame had inconsistent dimensions".to_owned())?;
    let (target_width, target_height) =
        *locked_dims.get_or_insert_with(|| target_dimensions(width, height, max_width));
    if frame.dimensions() == (target_width, target_height) {
        return Ok(frame);
    }
    if width.saturating_sub(target_width) <= 1 && height.saturating_sub(target_height) <= 1 {
        return Ok(image::imageops::crop_imm(&frame, 0, 0, target_width, target_height).to_image());
    }
    Ok(image::imageops::resize(
        &frame,
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    ))
}

/// Output dimensions: capped at `max_width`, aspect preserved, forced even
/// (libx264 yuv420p requires even dimensions; harmless for GIF).
fn target_dimensions(width: u32, height: u32, max_width: u32) -> (u32, u32) {
    let capped_width = width.min(max_width).max(2);
    let scaled_height =
        ((u64::from(height) * u64::from(capped_width)) / u64::from(width.max(1))).max(2) as u32;
    (capped_width & !1, scaled_height & !1)
}

fn ffmpeg_encode_args(
    width: u32,
    height: u32,
    frame_delay_ms: u32,
    frame_rate_fps: Option<u16>,
    format: ResolvedRecordFormat,
    out_path: &Path,
) -> Vec<std::ffi::OsString> {
    let delay = frame_delay_ms.max(1);
    let framerate = frame_rate_fps
        .map(|fps| fps.to_string())
        .unwrap_or_else(|| format!("1000/{delay}"));
    let mut args = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgba",
        "-video_size",
        &format!("{width}x{height}"),
        "-framerate",
        &framerate,
        "-i",
        "-",
    ]
    .iter()
    .map(std::ffi::OsString::from)
    .collect::<Vec<_>>();
    match format {
        ResolvedRecordFormat::Mp4 => args.extend(
            [
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-crf",
                "18",
                "-movflags",
                "+faststart",
            ]
            .iter()
            .map(std::ffi::OsString::from),
        ),
        ResolvedRecordFormat::WebP => args.extend(
            [
                "-an",
                "-c:v",
                "libwebp_anim",
                "-loop",
                "0",
                "-lossless",
                "0",
                "-q:v",
                "85",
                "-compression_level",
                "6",
            ]
            .iter()
            .map(std::ffi::OsString::from),
        ),
        ResolvedRecordFormat::Gif => unreachable!("GIF does not use ffmpeg"),
    }
    args.push(out_path.as_os_str().to_owned());
    args
}

pub(crate) fn normalize_record_fps(fps: u16) -> u16 {
    fps.clamp(RECORD_FPS_MIN, RECORD_FPS_MAX)
}

pub(crate) fn normalize_loop_record_speed_percent(percent: u16) -> u16 {
    percent.clamp(10, crate::MAX_LOOP_SPEED_PERCENT)
}

fn loop_record_speed_label(percent: u16) -> String {
    let percent = normalize_loop_record_speed_percent(percent);
    match percent {
        25 => "0.25x".to_owned(),
        50 => "0.5x".to_owned(),
        75 => "0.75x".to_owned(),
        100 => "1x".to_owned(),
        value if value % 100 == 0 => format!("{}x", value / 100),
        value => format!("{:.2}x", f32::from(value) / 100.0),
    }
}

fn frame_delay_ms_for_fps(fps: u16) -> u32 {
    let fps = normalize_record_fps(fps);
    ((1000.0 / f32::from(fps)).round() as u32).max(1)
}

fn frame_repeat_for_elapsed(elapsed: Duration, frame_delay_ms: u32) -> usize {
    let frame_delay_ms = frame_delay_ms.max(1) as f64;
    let repeat = (elapsed.as_secs_f64() * 1000.0 / frame_delay_ms).round() as usize;
    repeat.clamp(1, 300)
}

/// True when `ffmpeg -version` succeeds on PATH.
fn detect_ffmpeg() -> bool {
    ffmpeg_binary_responds("ffmpeg")
}

fn detect_ffmpeg_webp_anim() -> bool {
    ffmpeg_encoder_available("libwebp_anim")
}

fn ffmpeg_binary_responds(program: &str) -> bool {
    let mut command = Command::new(program);
    command
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_console_window(&mut command);
    command
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn ffmpeg_encoder_available(encoder: &str) -> bool {
    let mut command = Command::new("ffmpeg");
    command
        .args(["-hide_banner", "-v", "error", "-encoders"])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    hide_console_window(&mut command);
    command
        .output()
        .map(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains(encoder)
        })
        .unwrap_or(false)
}

fn copy_recording_file_to_clipboard(path: &Path) -> Result<(), String> {
    copy_file_to_clipboard(path)
}

#[cfg(windows)]
fn copy_file_to_clipboard(path: &Path) -> Result<(), String> {
    let script = r#"
Add-Type -AssemblyName System.Windows.Forms
$files = New-Object System.Collections.Specialized.StringCollection
[void]$files.Add($args[0])
[System.Windows.Forms.Clipboard]::SetFileDropList($files)
"#;
    let mut command = Command::new("powershell.exe");
    command
        .arg("-NoProfile")
        .arg("-STA")
        .arg("-Command")
        .arg(script)
        .arg(path.as_os_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_console_window(&mut command);
    command
        .status()
        .map_err(|err| format!("clipboard helper spawn: {err}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("clipboard helper exited with {status}"))
            }
        })
}

#[cfg(not(windows))]
fn copy_file_to_clipboard(_path: &Path) -> Result<(), String> {
    Err("file clipboard is currently Windows-only".to_owned())
}

/// Keeps spawned helpers (ffmpeg) from flashing a console window on Windows.
fn hide_console_window(_command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        _command.creation_flags(CREATE_NO_WINDOW);
    }
}

fn capture_file_base(time: chrono::DateTime<chrono::Local>) -> String {
    format!("{CAPTURE_FILE_PREFIX}_{}", time.format("%Y%m%d_%H%M%S"))
}

/// Builds a non-colliding `<dir>/bowecho_<stamp>[_<n>].<ext>` path.
fn unique_capture_path(dir: &Path, base: &str, extension: &str) -> PathBuf {
    let mut path = dir.join(format!("{base}.{extension}"));
    let mut counter = 2_u32;
    while path.exists() {
        path = dir.join(format!("{base}_{counter}.{extension}"));
        counter += 1;
    }
    path
}

fn new_capture_path(extension: &str) -> Result<PathBuf, String> {
    let dir = settings::screenshots_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
    Ok(unique_capture_path(
        &dir,
        &capture_file_base(chrono::Local::now()),
        extension,
    ))
}

/// Flattens an egui screenshot into straight RGBA bytes with opaque alpha.
/// (Window captures are opaque, so premultiplied vs straight is moot, but
/// some clipboard/encoder consumers choke on alpha < 255.)
fn rgba_bytes_from_color_image(image: &egui::ColorImage) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(image.pixels.len() * 4);
    for pixel in &image.pixels {
        rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), u8::MAX]);
    }
    rgba
}

fn save_capture_png(image: &egui::ColorImage) -> Result<PathBuf, String> {
    let path = new_capture_path("png")?;
    let rgba = rgba_bytes_from_color_image(image);
    image::save_buffer_with_format(
        &path,
        &rgba,
        image.size[0] as u32,
        image.size[1] as u32,
        image::ExtendedColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .map_err(|err| format!("{err}"))?;
    Ok(path)
}

/// Crops a physical-pixel screenshot to a rect given in egui points,
/// clamping to the image bounds (the screenshot covers the full window).
fn crop_color_image(
    image: &egui::ColorImage,
    rect: egui::Rect,
    pixels_per_point: f32,
) -> egui::ColorImage {
    let [width, height] = image.size;
    let scale = pixels_per_point.max(f32::EPSILON);
    let x0 = ((rect.min.x * scale).floor().max(0.0) as usize).min(width);
    let y0 = ((rect.min.y * scale).floor().max(0.0) as usize).min(height);
    let x1 = ((rect.max.x * scale).ceil().max(0.0) as usize).min(width);
    let y1 = ((rect.max.y * scale).ceil().max(0.0) as usize).min(height);
    if x1 <= x0 || y1 <= y0 {
        return image.clone();
    }
    image.region_by_pixels([x0, y0], [x1 - x0, y1 - y0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn capture_file_base_is_sortable_timestamp() {
        let time = chrono::Local
            .with_ymd_and_hms(2026, 6, 11, 5, 51, 9)
            .unwrap();
        assert_eq!(capture_file_base(time), "bowecho_20260611_055109");
    }

    #[test]
    fn unique_capture_path_skips_existing_files() {
        let dir =
            std::env::temp_dir().join(format!("bowecho_media_test_{}_unique", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = unique_capture_path(&dir, "shot", "png");
        std::fs::write(&first, b"x").unwrap();
        let second = unique_capture_path(&dir, "shot", "png");
        assert_ne!(first, second);
        assert!(
            second
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("shot_2")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rgba_bytes_force_opaque_alpha() {
        let image = egui::ColorImage::new(
            [2, 1],
            vec![
                egui::Color32::from_rgba_premultiplied(10, 20, 30, 128),
                egui::Color32::from_rgb(200, 100, 50),
            ],
        );
        let rgba = rgba_bytes_from_color_image(&image);
        assert_eq!(rgba.len(), 8);
        assert_eq!(rgba[3], 255);
        assert_eq!(rgba[7], 255);
        assert_eq!(&rgba[4..7], &[200, 100, 50]);
    }

    #[test]
    fn crop_clamps_to_image_bounds() {
        let pixels = (0..12)
            .map(|index| egui::Color32::from_gray(index as u8))
            .collect::<Vec<_>>();
        let image = egui::ColorImage::new([4, 3], pixels);
        let cropped = crop_color_image(
            &image,
            egui::Rect::from_min_max(egui::pos2(1.0, 1.0), egui::pos2(99.0, 99.0)),
            1.0,
        );
        assert_eq!(cropped.size, [3, 2]);
        assert_eq!(cropped.pixels[0], egui::Color32::from_gray(5));
    }

    #[test]
    fn degenerate_crop_returns_full_image() {
        let image = egui::ColorImage::new([2, 2], vec![egui::Color32::WHITE; 4]);
        let cropped = crop_color_image(
            &image,
            egui::Rect::from_min_max(egui::pos2(50.0, 50.0), egui::pos2(60.0, 60.0)),
            1.0,
        );
        assert_eq!(cropped.size, [2, 2]);
    }

    #[test]
    fn target_dimensions_cap_width_and_stay_even() {
        assert_eq!(target_dimensions(2560, 1440, 1280), (1280, 720));
        assert_eq!(target_dimensions(1501, 951, 1280), (1280, 810));
        assert_eq!(target_dimensions(640, 481, 1280), (640, 480));
        assert_eq!(target_dimensions(3, 3, 1280), (2, 2));
    }

    #[test]
    fn native_odd_sized_recording_crops_instead_of_resampling() {
        let pixels = (0..9)
            .map(|index| egui::Color32::from_rgb(index as u8, 0, 0))
            .collect::<Vec<_>>();
        let image = egui::ColorImage::new([3, 3], pixels);
        let mut locked_dims = None;

        let frame = scaled_record_frame(&image, u32::MAX, &mut locked_dims).unwrap();

        assert_eq!(frame.dimensions(), (2, 2));
        assert_eq!(frame.get_pixel(0, 0).0, [0, 0, 0, 255]);
        assert_eq!(frame.get_pixel(1, 1).0, [4, 0, 0, 255]);
    }

    #[test]
    fn ffmpeg_args_request_compatible_h264() {
        let args = ffmpeg_encode_args(
            1280,
            720,
            700,
            None,
            ResolvedRecordFormat::Mp4,
            Path::new("out.mp4"),
        );
        let args: Vec<String> = args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"rawvideo".to_owned()));
        assert!(args.contains(&"1280x720".to_owned()));
        assert!(args.contains(&"1000/700".to_owned()));
        assert!(args.contains(&"libx264".to_owned()));
        assert!(args.contains(&"yuv420p".to_owned()));
        assert_eq!(args.last(), Some(&"out.mp4".to_owned()));
    }

    #[test]
    fn ffmpeg_args_use_exact_free_record_fps_when_requested() {
        let args = ffmpeg_encode_args(
            1280,
            720,
            33,
            Some(30),
            ResolvedRecordFormat::Mp4,
            Path::new("out.mp4"),
        );
        let args: Vec<String> = args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();

        assert!(args.contains(&"30".to_owned()));
        assert!(!args.contains(&"1000/33".to_owned()));
    }

    #[test]
    fn ffmpeg_args_request_animated_webp_loop() {
        let args = ffmpeg_encode_args(
            1280,
            720,
            700,
            None,
            ResolvedRecordFormat::WebP,
            Path::new("out.webp"),
        );
        let args: Vec<String> = args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();

        assert!(args.contains(&"1000/700".to_owned()));
        assert!(args.contains(&"libwebp_anim".to_owned()));
        assert!(args.contains(&"-loop".to_owned()));
        assert!(args.contains(&"0".to_owned()));
        assert!(args.contains(&"85".to_owned()));
        assert_eq!(args.last(), Some(&"out.webp".to_owned()));
    }

    #[test]
    fn record_fps_is_clamped_and_converted_to_frame_delay() {
        assert_eq!(normalize_record_fps(0), 1);
        assert_eq!(normalize_record_fps(30), 30);
        assert_eq!(normalize_record_fps(500), 60);
        assert_eq!(frame_delay_ms_for_fps(30), 33);
    }

    #[test]
    fn record_settings_label_exposes_native_format_and_fps() {
        let media = MediaShare {
            record_size: RecordSize::Native,
            record_format: RecordFormat::Mp4,
            record_fps: 60,
            ..MediaShare::default()
        };

        assert_eq!(
            media.record_settings_label(1600),
            "native MP4 · loop 16x · free 60fps"
        );
    }

    #[test]
    fn loop_record_speed_labels_cover_fast_exports() {
        assert_eq!(loop_record_speed_label(25), "0.25x");
        assert_eq!(loop_record_speed_label(100), "1x");
        assert_eq!(loop_record_speed_label(1600), "16x");
        assert_eq!(loop_record_speed_label(6400), "64x");
    }

    #[test]
    fn full_resolution_mp4_preset_sets_native_mp4_export() {
        let mut media = MediaShare::default();

        assert!(!media.full_resolution_mp4_enabled());
        media.use_full_resolution_mp4_preset();

        assert!(media.full_resolution_mp4_enabled());
        let label = media.record_settings_label(100);
        assert!(label.contains("native MP4"));
        assert!(label.contains("30fps"));
    }

    #[test]
    fn late_free_record_captures_repeat_frames_to_preserve_wall_time() {
        assert_eq!(frame_repeat_for_elapsed(Duration::from_millis(16), 16), 1);
        assert_eq!(frame_repeat_for_elapsed(Duration::from_millis(33), 16), 2);
        assert_eq!(frame_repeat_for_elapsed(Duration::from_millis(100), 16), 6);
    }

    #[test]
    fn missing_ffmpeg_binary_is_detected_without_panic() {
        assert!(!ffmpeg_binary_responds(
            "bowecho-definitely-not-a-real-binary"
        ));
    }

    fn synthetic_frame(width: usize, height: usize, tint: u8) -> Arc<egui::ColorImage> {
        let pixels = (0..width * height)
            .map(|index| egui::Color32::from_rgb(tint, (index % 256) as u8, 64))
            .collect::<Vec<_>>();
        Arc::new(egui::ColorImage::new([width, height], pixels))
    }

    #[test]
    fn gif_encoder_writes_looping_animation_from_synthetic_frames() {
        use image::AnimationDecoder as _;

        let dir =
            std::env::temp_dir().join(format!("bowecho_media_test_{}_gif", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("loop.gif");

        let mut sink = GifSink::create(&path, 64, 700).unwrap();
        for tint in [0_u8, 96, 192] {
            sink.push(&synthetic_frame(96, 64, tint)).unwrap();
        }
        sink.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"GIF89a"));

        let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).unwrap();
        let frames = decoder.into_frames().collect_frames().unwrap();
        assert_eq!(frames.len(), 3);
        // Downscaled to the 64px cap with aspect preserved and even dims.
        assert_eq!(frames[0].buffer().dimensions(), (64, 42));
        let (numerator, denominator) = frames[0].delay().numer_denom_ms();
        assert_eq!(numerator / denominator.max(1), 700);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gif_sink_locks_dimensions_across_resized_frames() {
        let dir = std::env::temp_dir().join(format!(
            "bowecho_media_test_{}_gif_resize",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resize.gif");

        let mut sink = GifSink::create(&path, 128, 700).unwrap();
        sink.push(&synthetic_frame(120, 80, 10)).unwrap();
        // Simulated mid-recording window resize must not change output dims.
        sink.push(&synthetic_frame(200, 60, 20)).unwrap();
        sink.finish().unwrap();

        use image::AnimationDecoder as _;
        let bytes = std::fs::read(&path).unwrap();
        let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).unwrap();
        let frames = decoder.into_frames().collect_frames().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0].buffer().dimensions(),
            frames[1].buffer().dimensions()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
