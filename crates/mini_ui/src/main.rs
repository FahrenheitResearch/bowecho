//! miniDerecho — the radar-only companion app (docs/miniderecho-spec.md).
//! M0 walking skeleton (§13): live KTLX reflectivity on the lowest tilt,
//! drag-pan / wheel-zoom over a single textured quad, keyboard loop over a
//! byte-budgeted FrameRing — all background work on `ui_core` slots, and
//! the rusty-weather ingest stack structurally unreachable
//! (tests/dependency_firewall.rs).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod feed;
mod frame_ring;
mod map_view;
mod render_worker;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use data_source::sites::{self, SiteRef};
use eframe::egui;
use radar_core::{MomentType, RadarVolume};
use render2d::ViewportRasterOptions;

use feed::{FeedActivity, MiniFeed};
use frame_ring::{Frame, FrameRing};
use map_view::MapView;
use render_worker::{RenderMsg, RenderReq, RenderWorker};

/// M0 hardcoded scope (§13 Task 2): one site, one product, lowest tilt.
const M0_SITE_KEY: &str = "KTLX";
/// Desktop frame-history byte budget for M0 (§13 Task 4; the §7 table).
const DESKTOP_FRAME_BYTE_BUDGET: usize = 1024 * 1024 * 1024;
/// Playback timing (§13 Task 4): dwell per frame, hold at the loop end.
const PLAYBACK_DWELL: Duration = Duration::from_millis(350);
const PLAYBACK_END_HOLD: Duration = Duration::from_millis(700);
/// Rendered-quad texture cache depth (loop length 8 + gesture slack; the
/// §7 desktop loop-render-cache row is the M2 budget owner).
const TEXTURE_CACHE_DEPTH: usize = 12;
/// Idle repaint cadence so the 1 s poll tick keeps firing without input.
const IDLE_REPAINT: Duration = Duration::from_millis(200);

const MAP_BACKGROUND: egui::Color32 = egui::Color32::from_rgb(10, 13, 18);
const STATUS_TEXT: egui::Color32 = egui::Color32::from_rgb(235, 240, 246);
const STATUS_FILL: egui::Color32 = egui::Color32::from_rgba_premultiplied(10, 13, 18, 235);

fn main() -> eframe::Result {
    // B4 (miniderecho-spec §1): mini's caches/stores live under its own
    // namespace; it never touches BowEcho's config.json and never honors
    // BOWECHO_DATA_DIR.
    settings::set_storage_namespace(Some("miniderecho".to_owned()));
    let data_root = data_root();
    let cache_dir = data_root.join("volumes");

    eframe::run_native(
        "miniDerecho",
        native_options(),
        Box::new(move |cc| Ok(Box::new(MiniApp::new(cc, cache_dir)))),
    )
}

/// `MINIDERECHO_DATA_DIR` env override, else the platform namespace root
/// (mini reads its own env var — never `BOWECHO_DATA_DIR`).
fn data_root() -> PathBuf {
    if let Ok(dir) = std::env::var("MINIDERECHO_DATA_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    settings::storage_root_for_namespace("miniderecho")
        .unwrap_or_else(|| std::env::temp_dir().join("miniderecho"))
}

fn native_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("miniDerecho")
            .with_inner_size([1100.0, 800.0])
            .with_min_inner_size([480.0, 360.0]),
        ..Default::default()
    }
}

/// Lowest tilt carrying reflectivity data — M0's fixed cut selection.
fn lowest_reflectivity_cut(volume: &RadarVolume) -> Option<usize> {
    volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, cut)| {
            cut.moments
                .get(&MomentType::Reflectivity)
                .is_some_and(|grid| !grid.radial_indices.is_empty())
        })
        .min_by(|(_, a), (_, b)| a.elevation_deg.total_cmp(&b.elevation_deg))
        .map(|(index, _)| index)
}

/// Stable identity of "this decoded volume at this cut/product" — the unit
/// the texture cache and playback gating reason about. A full decode
/// replacing a preview is a new Arc, hence a new frame id and a re-render.
fn frame_id(volume: &Arc<RadarVolume>, cut_index: usize, moment: &MomentType) -> u64 {
    let mut hasher = DefaultHasher::new();
    (Arc::as_ptr(volume) as usize).hash(&mut hasher);
    cut_index.hash(&mut hasher);
    moment.hash(&mut hasher);
    hasher.finish()
}

/// Render-request identity: frame id + every raster option bit.
fn render_key(frame_id: u64, options: ViewportRasterOptions) -> u64 {
    let mut hasher = DefaultHasher::new();
    frame_id.hash(&mut hasher);
    options.width.hash(&mut hasher);
    options.height.hash(&mut hasher);
    options.radar_x_px.to_bits().hash(&mut hasher);
    options.radar_y_px.to_bits().hash(&mut hasher);
    options.km_per_px_x.to_bits().hash(&mut hasher);
    options.km_per_px_y.to_bits().hash(&mut hasher);
    options.rotation_rad.to_bits().hash(&mut hasher);
    hasher.finish()
}

struct TexEntry {
    key: u64,
    frame_id: u64,
    options: ViewportRasterOptions,
    texture: egui::TextureHandle,
}

/// Insertion-ordered rendered-quad cache; oldest evicted beyond depth.
#[derive(Default)]
struct TextureStore {
    entries: Vec<TexEntry>,
}

impl TextureStore {
    fn insert(&mut self, entry: TexEntry) {
        self.entries.retain(|existing| existing.key != entry.key);
        self.entries.push(entry);
        while self.entries.len() > TEXTURE_CACHE_DEPTH {
            self.entries.remove(0);
        }
    }

    fn get(&self, key: u64) -> Option<&TexEntry> {
        self.entries.iter().find(|entry| entry.key == key)
    }

    fn contains(&self, key: u64) -> bool {
        self.get(key).is_some()
    }

    /// Newest entry rendered from `frame_id` (any view) — the stale-quad
    /// fallback while the exact render catches up.
    fn newest_for_frame(&self, frame_id: u64) -> Option<&TexEntry> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.frame_id == frame_id)
    }
}

struct MiniApp {
    site: SiteRef,
    site_label: String,
    feed: MiniFeed,
    ring: FrameRing,
    render: RenderWorker,
    map: MapView,
    textures: TextureStore,
    /// The one in-flight/last request (newest-wins lane): key sent last.
    last_requested: Option<u64>,
    /// Metadata for outstanding requests so results can be installed.
    pending: Vec<(u64, u64, ViewportRasterOptions)>,
    /// Key of the quad drawn last frame — the fallback while the current
    /// frame's first render is still in flight.
    last_drawn: Option<u64>,
    last_advance: Option<Instant>,
    last_error: Option<String>,
}

impl MiniApp {
    fn new(cc: &eframe::CreationContext<'_>, cache_dir: PathBuf) -> Self {
        // Prove the Phase-1C API (§13 Task 2): the M0 site goes through
        // SiteRef::parse_settings_key + resolve.
        let site = SiteRef::parse_settings_key(M0_SITE_KEY);
        let site_label = sites::resolve(&site)
            .map(|record| record.label)
            .unwrap_or_else(|| M0_SITE_KEY.to_owned());
        let site_id = site.settings_key();
        Self {
            site,
            site_label,
            feed: MiniFeed::new(site_id, cache_dir),
            ring: FrameRing::new(DESKTOP_FRAME_BYTE_BUDGET),
            render: RenderWorker::spawn(cc.egui_ctx.clone()),
            map: MapView::default(),
            textures: TextureStore::default(),
            last_requested: None,
            pending: Vec::new(),
            last_drawn: None,
            last_advance: None,
            last_error: None,
        }
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context) {
        ctx.input(|input| {
            if input.key_pressed(egui::Key::Space) && !self.ring.is_empty() {
                let playing = !self.ring.playing();
                self.ring.set_playing(playing);
                self.last_advance = playing.then(Instant::now);
            }
            if input.key_pressed(egui::Key::ArrowLeft) {
                self.ring.step(-1);
            }
            if input.key_pressed(egui::Key::ArrowRight) {
                self.ring.step(1);
            }
            if input.key_pressed(egui::Key::L) {
                self.ring.jump_to_newest();
            }
        });
    }

    /// Playback advances only onto frames whose render is available at the
    /// current view — hold, don't jitter (§13 Task 4). While holding, the
    /// destination frame's render is requested so the hold resolves.
    fn drive_playback(&mut self, rect: egui::Rect, pixels_per_point: f32) {
        if !self.ring.playing() || self.ring.len() < 2 {
            return;
        }
        let now = Instant::now();
        let started = *self.last_advance.get_or_insert(now);
        let wrap_next = self.ring.next_index() == Some(0);
        let dwell = if wrap_next {
            PLAYBACK_END_HOLD
        } else {
            PLAYBACK_DWELL
        };
        if now.duration_since(started) < dwell {
            return;
        }
        let Some(next_index) = self.ring.next_index() else {
            return;
        };
        let options = self.map.raster_options(rect, pixels_per_point);
        let Some((next_key, _, _)) = self.frame_render_target(next_index, options) else {
            return;
        };
        if self.textures.contains(next_key) {
            self.ring.advance_wrapping();
            self.last_advance = Some(now);
        } else {
            // Hold on the current frame and pull the next one's render
            // through the lane (the displayed frame is already cached, so
            // this cannot starve it).
            self.request_frame_render(next_index, options);
        }
    }

    /// `(render key, frame id, cut)` for a ring index at these options.
    fn frame_render_target(
        &self,
        index: usize,
        options: ViewportRasterOptions,
    ) -> Option<(u64, u64, usize)> {
        let frame = self.ring.frames().get(index)?;
        let cut_index = lowest_reflectivity_cut(&frame.volume)?;
        let id = frame_id(&frame.volume, cut_index, &MomentType::Reflectivity);
        Some((render_key(id, options), id, cut_index))
    }

    /// Send one coalesced render request for a ring frame (newest-wins in
    /// the lane does the throttling); no-op when already cached/requested.
    fn request_frame_render(&mut self, index: usize, options: ViewportRasterOptions) {
        let Some((key, id, cut_index)) = self.frame_render_target(index, options) else {
            return;
        };
        if self.textures.contains(key)
            || self.last_requested == Some(key)
            || self.pending.iter().any(|(k, _, _)| *k == key)
        {
            return;
        }
        let volume = Arc::clone(&self.ring.frames()[index].volume);
        self.last_requested = Some(key);
        self.pending.push((key, id, options));
        while self.pending.len() > 8 {
            self.pending.remove(0);
        }
        self.render.request(RenderReq {
            volume,
            cut_index,
            moment: MomentType::Reflectivity,
            options,
            key,
        });
    }

    fn install_render_results(&mut self, ctx: &egui::Context) {
        for message in self.render.poll() {
            match message {
                RenderMsg::Done {
                    key,
                    width,
                    height,
                    pixels,
                } => {
                    let Some(position) = self.pending.iter().position(|(k, _, _)| *k == key) else {
                        // Stale result for a superseded request; unblock the
                        // standing request if it was this key.
                        if self.last_requested == Some(key) {
                            self.last_requested = None;
                        }
                        continue;
                    };
                    let (_, frame, options) = self.pending.remove(position);
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [width as usize, height as usize],
                        &pixels,
                    );
                    self.render.recycle(pixels);
                    let texture = ctx.load_texture(
                        format!("radar-{key:016x}"),
                        image,
                        egui::TextureOptions::LINEAR,
                    );
                    self.textures.insert(TexEntry {
                        key,
                        frame_id: frame,
                        options,
                        texture,
                    });
                }
                RenderMsg::Failed { key, error } => {
                    self.pending.retain(|(k, _, _)| *k != key);
                    self.last_error = Some(error);
                }
            }
        }
    }

    /// The displayed frame at the current view is always the standing
    /// request; issued every frame during gestures, coalesced by the lane.
    fn request_render_if_needed(&mut self, rect: egui::Rect, pixels_per_point: f32) {
        if self.ring.is_empty() {
            return;
        }
        let options = self.map.raster_options(rect, pixels_per_point);
        self.request_frame_render(self.ring.cursor(), options);
    }

    fn draw_radar(&mut self, painter: &egui::Painter, rect: egui::Rect, pixels_per_point: f32) {
        let current_options = self.map.raster_options(rect, pixels_per_point);
        let displayed = self.ring.current().and_then(|frame| {
            let cut_index = lowest_reflectivity_cut(&frame.volume)?;
            Some(frame_id(
                &frame.volume,
                cut_index,
                &MomentType::Reflectivity,
            ))
        });

        // Exact render for (frame, view) → best stale render of the frame →
        // whatever was on screen last frame (preview→full handoff).
        let entry = displayed
            .and_then(|id| {
                let exact = render_key(id, current_options);
                self.textures
                    .get(exact)
                    .or_else(|| self.textures.newest_for_frame(id))
            })
            .or_else(|| self.last_drawn.and_then(|key| self.textures.get(key)));

        if let Some(entry) = entry {
            let quad = map_view::anchored_texture_rect(
                rect,
                pixels_per_point,
                entry.options,
                current_options,
            );
            painter.image(
                entry.texture.id(),
                quad,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            self.last_drawn = Some(entry.key);
        }
    }

    /// One-line status truth (§13 Task 3.4): written at drain time, always
    /// DATA time of the displayed frame, never wall clock.
    fn status_line(&self) -> String {
        let mut line = match self.ring.current() {
            Some(frame) => {
                let elevation = lowest_reflectivity_cut(&frame.volume)
                    .and_then(|index| frame.volume.cuts.get(index))
                    .map(|cut| format!(" {:.1}°", cut.elevation_deg))
                    .unwrap_or_default();
                let mode = if self.ring.playing() {
                    "LOOP"
                } else if self.ring.at_live_edge() {
                    "LIVE"
                } else {
                    "PAUSED"
                };
                format!(
                    "{mode} · {} · REF{elevation} · {} · frame {}/{}",
                    self.site_label,
                    frame.time.format("%Y-%m-%d %H:%M:%SZ"),
                    self.ring.cursor() + 1,
                    self.ring.len(),
                )
            }
            None => format!("{} — fetching latest volume…", self.site_label),
        };
        match self.feed.activity() {
            FeedActivity::Fetching if self.ring.is_empty() => {}
            FeedActivity::Fetching => line.push_str(" · fetching…"),
            FeedActivity::Decoding => line.push_str(" · decoding…"),
            FeedActivity::Backfilling => line.push_str(" · backfilling loop…"),
            FeedActivity::Idle => {}
        }
        if let Some(error) = &self.last_error {
            line.push_str(&format!("  [{error}]"));
        }
        line
    }

    fn draw_status_strip(&self, painter: &egui::Painter, rect: egui::Rect) {
        let text = self.status_line();
        let font = egui::FontId::proportional(14.0);
        let galley = painter.layout_no_wrap(text, font, STATUS_TEXT);
        let padding = egui::vec2(8.0, 5.0);
        let strip = egui::Rect::from_min_size(
            rect.min + egui::vec2(8.0, 8.0),
            galley.size() + padding * 2.0,
        );
        painter.rect_filled(strip, 4.0, STATUS_FILL);
        painter.galley(strip.min + padding, galley, STATUS_TEXT);
    }
}

impl eframe::App for MiniApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // Feed first: drain-time installs, then status derives from state.
        let events = self.feed.tick(ctx);
        for volume in events.volumes {
            self.ring.install(Frame {
                time: volume.volume_time,
                site: self.site.clone(),
                volume,
            });
        }
        if let Some(error) = events.errors.into_iter().last() {
            self.last_error = Some(error);
        } else if !self.ring.is_empty() {
            self.last_error = None;
        }
        // Loop backfill starts once the first live volume is on screen.
        if events.live_full_arrived && !self.feed.backfill_started() {
            self.feed.start_backfill(ctx);
        }

        self.handle_keyboard(ctx);
        self.install_render_results(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show_inside(ui, |ui| {
                let rect = ui.max_rect();
                let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
                let pixels_per_point = ctx.pixels_per_point();

                let (zoom_delta, scroll_y, hover_pos) = ctx.input(|input| {
                    (
                        input.zoom_delta(),
                        input.smooth_scroll_delta.y,
                        input.pointer.hover_pos(),
                    )
                });
                let scroll_y = if response.hovered() { scroll_y } else { 0.0 };
                if let Some(intent) = map_view::map_intent(
                    response.drag_delta(),
                    zoom_delta,
                    scroll_y,
                    hover_pos,
                    rect,
                ) {
                    self.map.apply(intent, rect);
                }

                self.drive_playback(rect, pixels_per_point);
                self.request_render_if_needed(rect, pixels_per_point);

                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 0.0, MAP_BACKGROUND);
                self.draw_radar(&painter, rect, pixels_per_point);
                self.draw_status_strip(&painter, rect);
            });

        // Keep the 1 s poll tick and playback clock alive without input.
        let wake = if self.ring.playing() {
            Duration::from_millis(33)
        } else {
            IDLE_REPAINT
        };
        ctx.request_repaint_after(wake);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use radar_core::{ElevationCut, GateRange, MomentGrid, RadarSite};

    fn cut_with_ref(elevation_deg: f32, with_data: bool) -> ElevationCut {
        let mut cut = ElevationCut::new(elevation_deg, None);
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 1_000,
                gate_count: 4,
            },
            1.0,
            0.0,
            Some(0),
            Some(1),
        );
        if with_data {
            grid.push_u8_row_slice(0, &[10, 20, 30, 40]).expect("row");
        }
        cut.moments.insert(MomentType::Reflectivity, grid);
        cut
    }

    #[test]
    fn lowest_reflectivity_cut_skips_empty_and_moment_free_cuts() {
        let mut volume = RadarVolume::new(RadarSite::new("TST"), Utc::now());
        volume.cuts.push(ElevationCut::new(0.2, None)); // no moments at all
        volume.cuts.push(cut_with_ref(0.3, false)); // REF present but empty
        volume.cuts.push(cut_with_ref(1.4, true));
        volume.cuts.push(cut_with_ref(0.5, true)); // lowest WITH data
        assert_eq!(lowest_reflectivity_cut(&volume), Some(3));

        let empty = RadarVolume::new(RadarSite::new("TST"), Utc::now());
        assert_eq!(lowest_reflectivity_cut(&empty), None);
    }

    #[test]
    fn render_key_tracks_frame_and_every_option_field() {
        let volume = Arc::new(RadarVolume::new(RadarSite::new("TST"), Utc::now()));
        let id = frame_id(&volume, 0, &MomentType::Reflectivity);
        let options = ViewportRasterOptions {
            width: 800,
            height: 600,
            radar_x_px: 400.0,
            radar_y_px: 300.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
            rotation_rad: 0.0,
        };
        let key = render_key(id, options);
        assert_eq!(key, render_key(id, options), "stable for equal inputs");

        let panned = ViewportRasterOptions {
            radar_x_px: 401.0,
            ..options
        };
        assert_ne!(key, render_key(id, panned), "pan changes the key");

        let other_cut = frame_id(&volume, 1, &MomentType::Reflectivity);
        assert_ne!(key, render_key(other_cut, options), "cut changes the key");

        // A new Arc (full decode replacing a preview) is a new frame id.
        let replacement = Arc::new(RadarVolume::new(RadarSite::new("TST"), Utc::now()));
        assert_ne!(id, frame_id(&replacement, 0, &MomentType::Reflectivity));
    }
}
