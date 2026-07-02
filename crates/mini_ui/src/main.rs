//! miniDerecho — the radar-only companion app (docs/miniderecho-spec.md).
//! M1 (§11 as amended by §11a): the tile basemap on screen under the
//! radar, the always-visible control bar (site · product · tilt · loop
//! transport · GO LIVE), the curated product enum with capability-as-
//! value, the searchable site picker, MiniSettings on the B4 path, and
//! the first-run fallback chain (saved site → IP geolocation → KTLX) —
//! all background work on `ui_core` slots, the rusty-weather ingest stack
//! structurally unreachable (tests/dependency_firewall.rs).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod bar;
mod feed;
mod frame_ring;
mod geoloc;
mod map_view;
mod mini_settings;
mod products;
mod render_worker;
mod site_picker;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use data_source::sites::{self, SiteRef};
use eframe::egui;
use radar_core::RadarVolume;
use render2d::ViewportRasterOptions;
use ui_core::tiles::{TileLayer, TileLayerConfig, TileStyle};
use ui_core::worker_slot::WorkerSlot;

use bar::{BarAction, BarState};
use feed::{FeedActivity, MiniFeed};
use frame_ring::{Frame, FrameRing};
use geoloc::{GEOLOCATION_TIMEOUT, GeoLocator, IpApiGeolocator, SiteChoiceSource};
use map_view::MapView;
use mini_settings::MiniSettings;
use products::Product;
use render_worker::{RenderMsg, RenderReq, RenderWorker};
use site_picker::SitePicker;

/// Desktop frame-history byte budget (§13 Task 4; the §7 table).
const DESKTOP_FRAME_BYTE_BUDGET: usize = 1024 * 1024 * 1024;
/// Desktop tile-texture budget (§7 basemap row; BowEcho's value).
const DESKTOP_TILE_TEXTURES: usize = 220;
const TILE_WORKERS: usize = 4;
/// Playback timing (§13 Task 4): dwell per frame, hold at the loop end.
const PLAYBACK_DWELL: Duration = Duration::from_millis(350);
const PLAYBACK_END_HOLD: Duration = Duration::from_millis(700);
/// Rendered-quad texture cache depth (loop length 8 + gesture slack; the
/// §7 desktop loop-render-cache row is the M2 budget owner).
const TEXTURE_CACHE_DEPTH: usize = 12;
/// Idle repaint cadence so the 1 s poll tick keeps firing without input.
const IDLE_REPAINT: Duration = Duration::from_millis(200);
/// Radar raster tint over a tile basemap (§11a "sensible alpha"): opaque
/// enough to read meteorologically, open enough that roads/terrain
/// register underneath. Full-opacity on the tile-less dark background.
const RADAR_OVER_TILES_ALPHA: u8 = 217;

const MAP_BACKGROUND: egui::Color32 = egui::Color32::from_rgb(10, 13, 18);
const STATUS_TEXT: egui::Color32 = egui::Color32::from_rgb(235, 240, 246);
const STATUS_FILL: egui::Color32 = egui::Color32::from_rgba_premultiplied(10, 13, 18, 235);

fn main() -> eframe::Result {
    // B4 (miniderecho-spec §1): mini's caches/stores live under its own
    // namespace; it never touches BowEcho's config.json and never honors
    // BOWECHO_DATA_DIR.
    settings::set_storage_namespace(Some("miniderecho".to_owned()));
    let data_root = data_root();

    eframe::run_native(
        "miniDerecho",
        native_options(),
        Box::new(move |cc| Ok(Box::new(MiniApp::new(cc, data_root)))),
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

/// Stable identity of "this decoded volume at this cut/product" — the unit
/// the texture cache and playback gating reason about. A full decode
/// replacing a preview is a new Arc, hence a new frame id and a re-render.
fn frame_id(volume: &Arc<RadarVolume>, cut_index: usize, product: Product) -> u64 {
    let mut hasher = DefaultHasher::new();
    (Arc::as_ptr(volume) as usize).hash(&mut hasher);
    cut_index.hash(&mut hasher);
    product.hash(&mut hasher);
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

/// Everything bound to one selected site; replaced whole on site switch
/// (dropping the old feed's slots cancels its jobs — the drop-rx
/// contract).
struct Session {
    site: SiteRef,
    site_label: String,
    /// The AEQD projection center for tiles. `None` (no catalog coords)
    /// draws no basemap — honestly, not wrongly.
    radar_lat_lon: Option<(f64, f64)>,
    feed: MiniFeed,
    ring: FrameRing,
}

/// First-run boot state (§3.2): at most one geolocation wait, bounded by
/// [`GEOLOCATION_TIMEOUT`], then the pure chain picks the site.
enum Boot {
    Locating {
        slot: WorkerSlot<Option<(f32, f32)>>,
        started: Instant,
    },
    Done,
}

struct MiniApp {
    data_root: PathBuf,
    settings: MiniSettings,
    product: Product,
    tile_style: TileStyle,
    boot: Boot,
    session: Option<Session>,
    /// First-run geolocation fix, kept to order the site picker
    /// nearest-first.
    located: Option<(f32, f32)>,
    render: RenderWorker,
    map: MapView,
    tiles: TileLayer,
    picker: SitePicker,
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
    fn new(cc: &eframe::CreationContext<'_>, data_root: PathBuf) -> Self {
        let settings = MiniSettings::load();
        let product = Product::from_key(&settings.product);
        let tile_style = TileStyle::from_key(&settings.tile_style);
        let tiles = TileLayer::new(TileLayerConfig {
            cache_dir: Some(data_root.join("tiles")),
            max_textures: DESKTOP_TILE_TEXTURES,
            max_workers: TILE_WORKERS,
            debug_env: map_view::TILE_DEBUG_ENV,
        });
        let mut app = Self {
            data_root,
            product,
            tile_style,
            boot: Boot::Done,
            session: None,
            located: None,
            render: RenderWorker::spawn(cc.egui_ctx.clone()),
            map: MapView::default(),
            tiles,
            picker: SitePicker::default(),
            textures: TextureStore::default(),
            last_requested: None,
            pending: Vec::new(),
            last_drawn: None,
            last_advance: None,
            last_error: None,
            settings,
        };

        // First-run chain (§3.2): a resolvable saved site makes the
        // geolocation fetch first-run-only; the kill-switch skips it too.
        let saved = app.settings.last_site.clone();
        let immediate = geoloc::choose_startup_site(saved.as_deref(), None);
        if immediate.source == SiteChoiceSource::Saved || !app.settings.ip_geolocation {
            app.activate_site(immediate.site);
        } else {
            let mut slot = WorkerSlot::idle("mini geolocate");
            slot.spawn(&cc.egui_ctx, move |tx| {
                let _ = tx.send(IpApiGeolocator.locate());
            });
            app.boot = Boot::Locating {
                slot,
                started: Instant::now(),
            };
        }
        app
    }

    /// Drive the boot state machine: one bounded geolocation wait, then
    /// the pure chain (silent fallback — no dialogs, spec §3.2).
    fn boot_tick(&mut self) {
        if let Boot::Locating { slot, started } = &mut self.boot {
            let timed_out = started.elapsed() >= GEOLOCATION_TIMEOUT;
            if let Some(fix) = geoloc::locate_step(slot.poll(), timed_out) {
                self.located = fix;
                let choice = geoloc::choose_startup_site(self.settings.last_site.as_deref(), fix);
                self.boot = Boot::Done; // drops the slot ⇒ cancels a late fetch
                self.activate_site(choice.site);
            }
        }
    }

    /// Switch the app onto a site: fresh feed + ring (per-site volume
    /// cache dir, so warm launch never decodes another site's volume),
    /// view recentered on the radar, per-site render state cleared.
    fn activate_site(&mut self, site: SiteRef) {
        let record = sites::resolve(&site);
        let site_label = record
            .as_ref()
            .map(|record| record.label.clone())
            .unwrap_or_else(|| site.settings_key());
        let radar_lat_lon = record
            .as_ref()
            .and_then(|record| record.lat_lon)
            .map(|(lat, lon)| (f64::from(lat), f64::from(lon)));
        let site_id = site.settings_key();
        let cache_dir = self.data_root.join("volumes").join(&site_id);
        self.session = Some(Session {
            site: site.clone(),
            site_label,
            radar_lat_lon,
            feed: MiniFeed::new(site_id.clone(), cache_dir),
            ring: FrameRing::new(DESKTOP_FRAME_BYTE_BUDGET),
        });
        self.textures = TextureStore::default();
        self.pending.clear();
        self.last_requested = None;
        self.last_drawn = None;
        self.last_advance = None;
        self.last_error = None;
        self.map.center_east_km = 0.0;
        self.map.center_north_km = 0.0;
        if self.settings.last_site.as_deref() != Some(site_id.as_str()) {
            self.settings.last_site = Some(site_id);
            self.settings.save();
        }
    }

    /// Central dispatch for every bar/keyboard verb (the
    /// `UnifiedPlayerAction` pattern, spec §4). Transport verbs need a
    /// session; the settings verbs work from the boot screen too.
    fn apply_action(&mut self, session: Option<&mut Session>, action: BarAction) {
        match action {
            BarAction::OpenSitePicker => self.picker.open(),
            BarAction::SetProduct(product) => {
                self.product = product;
                self.settings.product = product.key().to_owned();
                self.settings.save();
            }
            BarAction::SetTilt(tenths) => {
                self.settings.tilt_deg_tenths = Some(tenths);
                self.settings.save();
            }
            BarAction::SetTileStyle(style) => {
                self.tile_style = style;
                self.settings.tile_style = style.key().to_owned();
                self.settings.save();
            }
            BarAction::SetGeolocEnabled(enabled) => {
                self.settings.ip_geolocation = enabled;
                self.settings.save();
            }
            BarAction::TogglePlay
            | BarAction::StepBack
            | BarAction::StepForward
            | BarAction::GoLive => {
                let Some(session) = session else {
                    return;
                };
                match action {
                    BarAction::TogglePlay => {
                        if !session.ring.is_empty() {
                            let playing = !session.ring.playing();
                            session.ring.set_playing(playing);
                            self.last_advance = playing.then(Instant::now);
                        }
                    }
                    BarAction::StepBack => session.ring.step(-1),
                    BarAction::StepForward => session.ring.step(1),
                    BarAction::GoLive => session.ring.jump_to_newest(),
                    _ => unreachable!("outer match narrowed to transport verbs"),
                }
            }
        }
    }

    /// Keyboard verbs (§4 desktop map): space, ←/→, L, ↑/↓ tilt, F find.
    fn keyboard_actions(&self, ctx: &egui::Context, session: &Session) -> Vec<BarAction> {
        if ctx.egui_wants_keyboard_input() {
            return Vec::new(); // the picker's search box is typing
        }
        let mut actions = Vec::new();
        ctx.input(|input| {
            if input.key_pressed(egui::Key::Space) {
                actions.push(BarAction::TogglePlay);
            }
            if input.key_pressed(egui::Key::ArrowLeft) {
                actions.push(BarAction::StepBack);
            }
            if input.key_pressed(egui::Key::ArrowRight) {
                actions.push(BarAction::StepForward);
            }
            if input.key_pressed(egui::Key::L) {
                actions.push(BarAction::GoLive);
            }
            if input.key_pressed(egui::Key::F) {
                actions.push(BarAction::OpenSitePicker);
            }
            let tilt_step = i64::from(input.key_pressed(egui::Key::ArrowUp))
                - i64::from(input.key_pressed(egui::Key::ArrowDown));
            if tilt_step != 0
                && let Some(tenths) = self.stepped_tilt(session, tilt_step)
            {
                actions.push(BarAction::SetTilt(tenths));
            }
        });
        actions
    }

    /// The tilt one step above/below the current selection, as a
    /// preference value.
    fn stepped_tilt(&self, session: &Session, step: i64) -> Option<i32> {
        let frame = session.ring.current()?;
        let tilts = products::capability(&frame.volume, self.product).ok()?;
        let selected = products::select_tilt(&tilts, self.settings.tilt_deg_tenths)?;
        let index = tilts
            .iter()
            .position(|tilt| tilt.cut_index == selected.cut_index)?;
        let stepped = (index as i64 + step).clamp(0, tilts.len() as i64 - 1) as usize;
        Some(products::elevation_tenths(tilts[stepped].elevation_deg))
    }

    /// Playback advances only onto frames whose render is available at the
    /// current view — hold, don't jitter (§13 Task 4). While holding, the
    /// destination frame's render is requested so the hold resolves.
    fn drive_playback(&mut self, session: &mut Session, rect: egui::Rect, pixels_per_point: f32) {
        if !session.ring.playing() || session.ring.len() < 2 {
            return;
        }
        let now = Instant::now();
        let started = *self.last_advance.get_or_insert(now);
        let wrap_next = session.ring.next_index() == Some(0);
        let dwell = if wrap_next {
            PLAYBACK_END_HOLD
        } else {
            PLAYBACK_DWELL
        };
        if now.duration_since(started) < dwell {
            return;
        }
        let Some(next_index) = session.ring.next_index() else {
            return;
        };
        let options = self.map.raster_options(rect, pixels_per_point);
        let Some((next_key, _, _)) = self.frame_render_target(session, next_index, options) else {
            return;
        };
        if self.textures.contains(next_key) {
            session.ring.advance_wrapping();
            self.last_advance = Some(now);
        } else {
            // Hold on the current frame and pull the next one's render
            // through the lane (the displayed frame is already cached, so
            // this cannot starve it).
            self.request_frame_render(session, next_index, options);
        }
    }

    /// `(render key, frame id, cut)` for a ring index at these options,
    /// under the current product + tilt preference (capability-as-value:
    /// `None` when this frame's volume cannot render the product).
    fn frame_render_target(
        &self,
        session: &Session,
        index: usize,
        options: ViewportRasterOptions,
    ) -> Option<(u64, u64, usize)> {
        let frame = session.ring.frames().get(index)?;
        let tilts = products::capability(&frame.volume, self.product).ok()?;
        let tilt = products::select_tilt(&tilts, self.settings.tilt_deg_tenths)?;
        let id = frame_id(&frame.volume, tilt.cut_index, self.product);
        Some((render_key(id, options), id, tilt.cut_index))
    }

    /// Send one coalesced render request for a ring frame (newest-wins in
    /// the lane does the throttling); no-op when already cached/requested.
    fn request_frame_render(
        &mut self,
        session: &Session,
        index: usize,
        options: ViewportRasterOptions,
    ) {
        let Some((key, id, cut_index)) = self.frame_render_target(session, index, options) else {
            return;
        };
        if self.textures.contains(key)
            || self.last_requested == Some(key)
            || self.pending.iter().any(|(k, _, _)| *k == key)
        {
            return;
        }
        let volume = Arc::clone(&session.ring.frames()[index].volume);
        self.last_requested = Some(key);
        self.pending.push((key, id, options));
        while self.pending.len() > 8 {
            self.pending.remove(0);
        }
        self.render.request(RenderReq {
            volume,
            cut_index,
            product: self.product.render_product(),
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
    fn request_render_if_needed(
        &mut self,
        session: &Session,
        rect: egui::Rect,
        pixels_per_point: f32,
    ) {
        if session.ring.is_empty() {
            return;
        }
        let options = self.map.raster_options(rect, pixels_per_point);
        self.request_frame_render(session, session.ring.cursor(), options);
    }

    fn draw_radar(
        &mut self,
        session: &Session,
        painter: &egui::Painter,
        rect: egui::Rect,
        pixels_per_point: f32,
        over_tiles: bool,
    ) {
        let current_options = self.map.raster_options(rect, pixels_per_point);
        let displayed = session.ring.current().and_then(|frame| {
            let tilts = products::capability(&frame.volume, self.product).ok()?;
            let tilt = products::select_tilt(&tilts, self.settings.tilt_deg_tenths)?;
            Some(frame_id(&frame.volume, tilt.cut_index, self.product))
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
            let tint = if over_tiles {
                egui::Color32::from_white_alpha(RADAR_OVER_TILES_ALPHA)
            } else {
                egui::Color32::WHITE
            };
            painter.image(
                entry.texture.id(),
                quad,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                tint,
            );
            self.last_drawn = Some(entry.key);
        }
    }

    /// One-line status truth (§13 Task 3.4): written at drain time, always
    /// DATA time of the displayed frame, never wall clock.
    fn status_line(&self, session: &Session) -> String {
        let mut line = match session.ring.current() {
            Some(frame) => {
                let product_bit = match products::capability(&frame.volume, self.product) {
                    Ok(tilts) => {
                        let elevation =
                            products::select_tilt(&tilts, self.settings.tilt_deg_tenths)
                                .map(|tilt| format!(" {:.1}°", tilt.elevation_deg))
                                .unwrap_or_default();
                        format!("{}{elevation}", self.product.label())
                    }
                    // Capability-as-value: the grey reason IS the status.
                    Err(reason) => format!("{} unavailable: {reason}", self.product.label()),
                };
                let mode = if session.ring.playing() {
                    "LOOP"
                } else if session.ring.at_live_edge() {
                    "LIVE"
                } else {
                    "PAUSED"
                };
                format!(
                    "{mode} · {} · {product_bit} · {} · frame {}/{}",
                    session.site_label,
                    frame.time.format("%Y-%m-%d %H:%M:%SZ"),
                    session.ring.cursor() + 1,
                    session.ring.len(),
                )
            }
            None => format!("{} — fetching latest volume…", session.site_label),
        };
        match session.feed.activity() {
            FeedActivity::Fetching if session.ring.is_empty() => {}
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

    fn draw_status_strip(&self, painter: &egui::Painter, rect: egui::Rect, text: String) {
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

    /// Snapshot for the bar (drain-time truth: capability values computed
    /// against the DISPLAYED volume).
    fn bar_state<'a>(
        &self,
        session: Option<&'a Session>,
        tilts: &'a [products::TiltOption],
    ) -> BarState<'a> {
        let current = session.and_then(|session| session.ring.current());
        let products_rows = Product::ALL
            .into_iter()
            .map(|product| {
                let capability = match current {
                    Some(frame) => products::capability(&frame.volume, product).map(|_| ()),
                    None => Err("no data yet".to_owned()),
                };
                (product, capability)
            })
            .collect();
        BarState {
            site_label: session.map_or("Locating…", |session| session.site_label.as_str()),
            site_enabled: session.is_some(),
            product: self.product,
            products: products_rows,
            tilts,
            selected_tilt: products::select_tilt(tilts, self.settings.tilt_deg_tenths),
            playing: session.is_some_and(|session| session.ring.playing()),
            at_live_edge: session.is_some_and(|session| session.ring.at_live_edge()),
            frame_position: session.and_then(|session| {
                (!session.ring.is_empty()).then(|| (session.ring.cursor() + 1, session.ring.len()))
            }),
            data_time: current.map(|frame| frame.time.format("%H:%M:%SZ").to_string()),
            tile_style: self.tile_style,
            ip_geolocation: self.settings.ip_geolocation,
        }
    }

    /// The picker's "near" location: the first-run fix when there was
    /// one, else the current site's pad.
    fn picker_location(&self, session: Option<&Session>) -> Option<(f32, f32)> {
        self.located.or_else(|| {
            session
                .and_then(|session| session.radar_lat_lon)
                .map(|(lat, lon)| (lat as f32, lon as f32))
        })
    }

    /// The map itself: gestures, playback, renders, tiles under radar.
    fn map_panel(&mut self, session: &mut Session, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
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
        if let Some(intent) =
            map_view::map_intent(response.drag_delta(), zoom_delta, scroll_y, hover_pos, rect)
        {
            self.map.apply(intent, rect);
        }

        self.drive_playback(session, rect, pixels_per_point);
        self.request_render_if_needed(session, rect, pixels_per_point);

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, MAP_BACKGROUND);
        self.tiles.poll(&ctx);
        let mut over_tiles = false;
        if let Some(radar) = session.radar_lat_lon
            && self.tile_style != TileStyle::DarkVector
        {
            map_view::draw_tile_basemap(
                &mut self.tiles,
                &painter,
                rect,
                &self.map,
                radar,
                self.tile_style,
                pixels_per_point,
            );
            over_tiles = true;
        }
        self.draw_radar(session, &painter, rect, pixels_per_point, over_tiles);
        let status = self.status_line(session);
        self.draw_status_strip(&painter, rect, status);
    }
}

impl eframe::App for MiniApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.boot_tick();

        let mut session = self.session.take();

        // Feed first: drain-time installs, then status derives from state.
        if let Some(session) = session.as_mut() {
            let events = session.feed.tick(ctx);
            for volume in events.volumes {
                session.ring.install(Frame {
                    time: volume.volume_time,
                    site: session.site.clone(),
                    volume,
                });
            }
            if let Some(error) = events.errors.into_iter().last() {
                self.last_error = Some(error);
            } else if !session.ring.is_empty() {
                self.last_error = None;
            }
            // Loop backfill starts once the first live volume is on screen.
            if events.live_full_arrived && !session.feed.backfill_started() {
                session.feed.start_backfill(ctx);
            }
        }

        self.install_render_results(ctx);

        // THE BAR (§11a): always visible — top controls + transport strip.
        let tilts = session
            .as_ref()
            .and_then(|session| session.ring.current())
            .map(|frame| products::capability(&frame.volume, self.product).unwrap_or_default())
            .unwrap_or_default();
        let state = self.bar_state(session.as_ref(), &tilts);
        let mut actions = bar::top_bar(ui, &state);
        actions.extend(bar::transport_bar(ui, &state));
        if let Some(session) = session.as_ref() {
            actions.extend(self.keyboard_actions(ctx, session));
        }

        // SITE PICKER window (opened by the bar's SITE control / F).
        let picked = self
            .picker
            .show(ctx, self.picker_location(session.as_ref()));

        for action in actions {
            self.apply_action(session.as_mut(), action);
        }

        match session.as_mut() {
            Some(session) => {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show_inside(ui, |ui| self.map_panel(session, ui));
            }
            None => {
                // Boot screen (≤ 2 s): dark map + honest status text.
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show_inside(ui, |ui| {
                        let rect = ui.max_rect();
                        let painter = ui.painter_at(rect);
                        painter.rect_filled(rect, 0.0, MAP_BACKGROUND);
                        self.draw_status_strip(
                            &painter,
                            rect,
                            "Locating nearest radar…".to_owned(),
                        );
                    });
            }
        }
        self.session = session;
        if let Some(site) = picked {
            // The old session (feed slots included) drops here — the
            // drop-rx contract cancels its jobs.
            self.activate_site(site);
        }

        // Keep the poll tick, playback clock, and boot timeout alive
        // without input.
        let playing = self
            .session
            .as_ref()
            .is_some_and(|session| session.ring.playing());
        let wake = if playing {
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
    use radar_core::RadarSite;

    #[test]
    fn render_key_tracks_frame_and_every_option_field() {
        let volume = Arc::new(RadarVolume::new(RadarSite::new("TST"), Utc::now()));
        let id = frame_id(&volume, 0, Product::Ref);
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

        let other_cut = frame_id(&volume, 1, Product::Ref);
        assert_ne!(key, render_key(other_cut, options), "cut changes the key");

        // Product switches re-render: VEL and DVEL on the same cut are
        // different frames to the texture cache.
        assert_ne!(
            frame_id(&volume, 0, Product::Vel),
            frame_id(&volume, 0, Product::DealiasedVel)
        );

        // A new Arc (full decode replacing a preview) is a new frame id.
        let replacement = Arc::new(RadarVolume::new(RadarSite::new("TST"), Utc::now()));
        assert_ne!(id, frame_id(&replacement, 0, Product::Ref));
    }
}
