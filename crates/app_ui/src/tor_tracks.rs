//! TOR TRACKS map layers: rotation-tracks accumulation + TDS flags.
//!
//! Rotation tracks: per-cell MAXIMUM low-level cyclonic azimuthal shear
//! accumulated over the loaded frame history — the swath a translating
//! mesocyclone paints (MRMS rotation-tracks lineage: Mahalik et al. 2019,
//! Wea. Forecasting 34, doi:10.1175/WAF-D-18-0165.1; Miller et al. 2013,
//! 28th Conf. IIPS; Smith et al. 2016, BAMS 97,
//! doi:10.1175/BAMS-D-14-00173.1). TDS flags: deterministic dual-pol debris
//! criteria (Ryzhkov et al. 2005; Van Den Broeke & Jauernic 2014; Snyder &
//! Ryzhkov 2015) — never a probability.
//!
//! Loop semantics: scrubbing the history shows the accumulation UP TO the
//! viewed frame (per-frame Cartesian grids are kept and max-composited on
//! demand; forward steps fold incrementally, backward jumps refold — ≤ 30
//! frames × 360k cells stays well under a frame budget). New live frames are
//! picked up by the per-update poll and accumulate as they decode; "Reset"
//! restarts the window at the newest loaded frame.
//!
//! All math lives in `render2d::tracks`; this module is state + paint.

use chrono::{DateTime, Utc};
use eframe::egui;
use render2d::detect_rotation_sites_from_dealiased;
use render2d::tracks::{
    TdsGate, TracksGridSpec, detect_tds_gates, low_level_azshear_cartesian_from_dealiased,
    low_level_azshear_cut_indices, max_composite_into, rotation_track_color,
};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

const TRACK_CACHE_MAGIC: &[u8; 8] = b"BETRK001";
const TRACK_CACHE_MAX_CELLS: usize = 1_000_000;
const TRACK_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// One processed history frame: its Cartesian low-level shear grid and TDS
/// gates, keyed by scan time + the volume allocation that produced them (a
/// replaced live-partial volume re-processes).
struct TrackFrame {
    scan_time: DateTime<Utc>,
    volume_ptr: usize,
    context: crate::DealiasContextKey,
    grid: Vec<f32>,
    tds: Vec<TdsGate>,
    /// A clean-exit max composite restored from disk. It is retained while
    /// newly loaded volume-backed frames reconcile, then participates as the
    /// accumulation baseline until Reset or a radar-site switch.
    persisted: bool,
}

#[derive(Clone, Debug)]
struct RotationTrackCache {
    site_id: String,
    site_lat: f32,
    site_lon: f32,
    spec: TracksGridSpec,
    upto: DateTime<Utc>,
    grid: Vec<f32>,
}

fn rotation_track_cache_path() -> std::path::PathBuf {
    crate::track_accumulation_cache_dir().join("rotation-tracks.bin")
}

fn encode_rotation_track_cache(cache: &RotationTrackCache) -> Option<Vec<u8>> {
    let site = cache.site_id.as_bytes();
    if site.is_empty()
        || site.len() > u16::MAX as usize
        || cache.grid.len() != cache.spec.cell_count()
        || cache.grid.len() > TRACK_CACHE_MAX_CELLS
    {
        return None;
    }
    let mut out = Vec::with_capacity(42 + site.len() + cache.grid.len() * 4);
    out.extend_from_slice(TRACK_CACHE_MAGIC);
    out.extend_from_slice(&(site.len() as u16).to_le_bytes());
    out.extend_from_slice(site);
    for value in [
        cache.site_lat,
        cache.site_lon,
        cache.spec.half_extent_km,
        cache.spec.cell_km,
    ] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&cache.upto.timestamp_millis().to_le_bytes());
    out.extend_from_slice(&(cache.grid.len() as u32).to_le_bytes());
    for value in &cache.grid {
        out.extend_from_slice(&value.to_le_bytes());
    }
    (out.len() <= TRACK_CACHE_MAX_BYTES).then_some(out)
}

fn decode_rotation_track_cache(bytes: &[u8]) -> Option<RotationTrackCache> {
    if bytes.len() > TRACK_CACHE_MAX_BYTES || bytes.get(..8)? != TRACK_CACHE_MAGIC {
        return None;
    }
    let mut cursor = 8usize;
    let take = |cursor: &mut usize, count: usize| -> Option<&[u8]> {
        let end = cursor.checked_add(count)?;
        let slice = bytes.get(*cursor..end)?;
        *cursor = end;
        Some(slice)
    };
    let site_len = u16::from_le_bytes(take(&mut cursor, 2)?.try_into().ok()?) as usize;
    let site_id = std::str::from_utf8(take(&mut cursor, site_len)?)
        .ok()?
        .trim()
        .to_owned();
    if site_id.is_empty() {
        return None;
    }
    let mut read_f32 = || Some(f32::from_le_bytes(take(&mut cursor, 4)?.try_into().ok()?));
    let site_lat = read_f32()?;
    let site_lon = read_f32()?;
    let half_extent_km = read_f32()?;
    let cell_km = read_f32()?;
    let upto_ms = i64::from_le_bytes(take(&mut cursor, 8)?.try_into().ok()?);
    let grid_len = u32::from_le_bytes(take(&mut cursor, 4)?.try_into().ok()?) as usize;
    let spec = TracksGridSpec {
        half_extent_km,
        cell_km,
    };
    if !site_lat.is_finite()
        || !site_lon.is_finite()
        || !half_extent_km.is_finite()
        || !cell_km.is_finite()
        || half_extent_km <= 0.0
        || cell_km <= 0.0
        || grid_len == 0
        || grid_len > TRACK_CACHE_MAX_CELLS
        || grid_len != spec.cell_count()
    {
        return None;
    }
    let raw_grid = take(&mut cursor, grid_len.checked_mul(4)?)?;
    if cursor != bytes.len() {
        return None;
    }
    let grid = raw_grid
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four-byte chunk")))
        .collect();
    Some(RotationTrackCache {
        site_id,
        site_lat,
        site_lon,
        spec,
        upto: DateTime::from_timestamp_millis(upto_ms)?,
        grid,
    })
}

fn load_rotation_track_cache(expected_site: &str) -> Option<RotationTrackCache> {
    let bytes = std::fs::read(rotation_track_cache_path()).ok()?;
    let cache = decode_rotation_track_cache(&bytes)?;
    cache
        .site_id
        .eq_ignore_ascii_case(expected_site)
        .then_some(cache)
}

fn write_rotation_track_cache(cache: &RotationTrackCache) -> Result<(), String> {
    let bytes = encode_rotation_track_cache(cache)
        .ok_or_else(|| "rotation-track cache is invalid or too large".to_owned())?;
    let path = rotation_track_cache_path();
    let parent = path
        .parent()
        .ok_or_else(|| "rotation-track cache path has no parent".to_owned())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("bin.tmp");
    std::fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|error| error.to_string())?;
    }
    std::fs::rename(temporary, path).map_err(|error| error.to_string())
}

struct TrackJobResult {
    site_id: String,
    site_lat: f32,
    site_lon: f32,
    scan_time: DateTime<Utc>,
    volume_ptr: usize,
    context: crate::DealiasContextKey,
    grid: Vec<f32>,
    tds: Vec<TdsGate>,
}

struct TrackJob {
    rx: mpsc::Receiver<TrackJobResult>,
}

/// Max-composite of the eligible frames up to the viewed frame, plus its
/// uploaded texture.
struct TracksDisplay {
    generation: u64,
    upto: DateTime<Utc>,
    frames_folded: usize,
    grid: Vec<f32>,
    texture: Option<egui::TextureHandle>,
}

pub(crate) struct TorTracksState {
    pub show_tracks: bool,
    pub show_tds: bool,
    pub tracks_opacity: f32,
    spec: TracksGridSpec,
    site_id: Option<String>,
    site_lat: f32,
    site_lon: f32,
    /// Ascending scan time.
    frames: Vec<TrackFrame>,
    job: Option<TrackJob>,
    /// Accumulation window floor ("Reset tracks" moves it to the newest
    /// loaded frame); `None` = the whole loaded history.
    reset_floor: Option<DateTime<Utc>>,
    /// Bumped whenever `frames`/floor change — invalidates the display fold.
    generation: u64,
    display: Option<TracksDisplay>,
}

impl Default for TorTracksState {
    fn default() -> Self {
        Self {
            show_tracks: false,
            show_tds: false,
            tracks_opacity: 0.85,
            spec: TracksGridSpec::default(),
            site_id: None,
            site_lat: f32::NAN,
            site_lon: f32::NAN,
            frames: Vec::new(),
            job: None,
            reset_floor: None,
            generation: 0,
            display: None,
        }
    }
}

impl TorTracksState {
    pub(crate) fn with_persisted(expected_site: &str, show_tracks: bool, show_tds: bool) -> Self {
        let mut state = Self {
            show_tracks,
            show_tds,
            ..Self::default()
        };
        if let Some(cache) = load_rotation_track_cache(expected_site) {
            state.spec = cache.spec;
            state.site_id = Some(cache.site_id);
            state.site_lat = cache.site_lat;
            state.site_lon = cache.site_lon;
            state.frames.push(TrackFrame {
                scan_time: cache.upto,
                volume_ptr: 0,
                context: crate::DealiasContextKey::new(crate::DealiasEngine::Region, None, None),
                grid: cache.grid,
                tds: Vec::new(),
                persisted: true,
            });
            state.generation = 1;
        }
        state
    }

    pub(crate) fn save_persisted_cache(&self) {
        let Some(site_id) = self.site_id.as_deref() else {
            return;
        };
        let Some(upto) = self
            .frames
            .iter()
            .filter(|frame| self.in_window(frame.scan_time))
            .map(|frame| frame.scan_time)
            .max()
        else {
            return;
        };
        let mut grid = vec![f32::NAN; self.spec.cell_count()];
        for frame in &self.frames {
            if frame.scan_time <= upto && self.in_window(frame.scan_time) {
                max_composite_into(&mut grid, &frame.grid);
            }
        }
        if !grid.iter().any(|value| value.is_finite()) {
            return;
        }
        let _ = write_rotation_track_cache(&RotationTrackCache {
            site_id: site_id.to_owned(),
            site_lat: self.site_lat,
            site_lon: self.site_lon,
            spec: self.spec,
            upto,
            grid,
        });
    }

    fn in_window(&self, scan_time: DateTime<Utc>) -> bool {
        self.reset_floor.is_none_or(|floor| scan_time >= floor)
    }

    fn clear_frames(&mut self) {
        self.frames.clear();
        self.display = None;
        self.site_id = None;
        // A reset taken on one site/case must not exclude another's frames
        // (an archive case is usually older than a live reset floor).
        self.reset_floor = None;
        self.generation = self.generation.wrapping_add(1);
    }

    fn has_frame(
        &self,
        scan_time: DateTime<Utc>,
        volume_ptr: usize,
        context: crate::DealiasContextKey,
    ) -> bool {
        self.frames.iter().any(|frame| {
            frame.scan_time == scan_time
                && frame.volume_ptr == volume_ptr
                && frame.context == context
        })
    }
}

impl crate::ViewerApp {
    pub(crate) fn set_tor_track_layers_visible(&mut self, show_tracks: bool, show_tds: bool) {
        self.tor_tracks.show_tracks = show_tracks;
        self.tor_tracks.show_tds = show_tds;
        if self.app_settings.rotation_tracks_enabled != show_tracks
            || self.app_settings.tds_tracks_enabled != show_tds
        {
            self.app_settings.rotation_tracks_enabled = show_tracks;
            self.app_settings.tds_tracks_enabled = show_tds;
            self.mark_app_settings_dirty();
        }
    }

    /// Per-update pump: install finished background work, reconcile the
    /// processed frames with the loaded history (site switch / trimmed loop /
    /// re-decoded live frames), kick the next background job, and keep the
    /// displayed max-composite in sync with the viewed frame.
    pub(crate) fn poll_tor_tracks(&mut self, ctx: &egui::Context) {
        // 1. Install a finished job.
        let mut finished = None;
        if let Some(job) = &self.tor_tracks.job {
            match job.rx.try_recv() {
                Ok(result) => {
                    finished = Some(result);
                    self.tor_tracks.job = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.tor_tracks.job = None,
            }
        }

        if !self.tor_tracks.show_tracks && !self.tor_tracks.show_tds {
            return;
        }

        // 2. Reconcile with the loaded history. The history is single-site;
        //    a site switch resets the accumulation.
        let active_site = self
            .volume
            .as_ref()
            .map(|volume| volume.site.id.clone())
            .or_else(|| {
                self.primary
                    .history
                    .last()
                    .map(|frame| frame.identity.site_id.clone())
            });
        let Some(active_site) = active_site else {
            return;
        };
        if self
            .tor_tracks
            .site_id
            .as_ref()
            .is_some_and(|site| *site != active_site)
        {
            self.tor_tracks.clear_frames();
        }
        let valid: Vec<(DateTime<Utc>, usize, crate::DealiasContextKey)> = self
            .primary
            .history
            .iter()
            .filter(|frame| frame.identity.site_id == active_site)
            .map(|frame| {
                let (_, _, context) = self.primary_dealias_inputs_for_volume(&frame.volume);
                (
                    frame.identity.scan_time_utc,
                    Arc::as_ptr(&frame.volume) as usize,
                    context,
                )
            })
            .collect();
        let before = self.tor_tracks.frames.len();
        self.tor_tracks.frames.retain(|frame| {
            frame.persisted || valid.contains(&(frame.scan_time, frame.volume_ptr, frame.context))
        });
        if self.tor_tracks.frames.len() != before {
            self.tor_tracks.generation = self.tor_tracks.generation.wrapping_add(1);
        }

        // 3. Install the finished result (post-reconcile so a stale-site
        //    result never sneaks in).
        if let Some(result) = finished {
            if result.site_id == active_site
                && valid.contains(&(result.scan_time, result.volume_ptr, result.context))
            {
                let state = &mut self.tor_tracks;
                state.site_id = Some(result.site_id);
                state.site_lat = result.site_lat;
                state.site_lon = result.site_lon;
                state
                    .frames
                    .retain(|frame| frame.persisted || frame.scan_time != result.scan_time);
                let at = state
                    .frames
                    .partition_point(|frame| frame.scan_time < result.scan_time);
                state.frames.insert(
                    at,
                    TrackFrame {
                        scan_time: result.scan_time,
                        volume_ptr: result.volume_ptr,
                        context: result.context,
                        grid: result.grid,
                        tds: result.tds,
                        persisted: false,
                    },
                );
                state.generation = state.generation.wrapping_add(1);
            }
            ctx.request_repaint();
        }

        // 4. Kick the next job (one at a time, oldest eligible first so the
        //    swath builds chronologically).
        if self.tor_tracks.job.is_none() {
            let next = self.primary.history.iter().find_map(|frame| {
                if frame.identity.site_id != active_site
                    || frame.status == crate::FrameStatus::Preview
                    || !self.tor_tracks.in_window(frame.identity.scan_time_utc)
                {
                    return None;
                }
                let (previous_volume, dealias_env, context) =
                    self.primary_dealias_inputs_for_volume(&frame.volume);
                let volume_ptr = Arc::as_ptr(&frame.volume) as usize;
                if self
                    .tor_tracks
                    .has_frame(frame.identity.scan_time_utc, volume_ptr, context)
                {
                    return None;
                }
                let (Some(site_lat), Some(site_lon)) = (
                    frame.volume.site.latitude_deg,
                    frame.volume.site.longitude_deg,
                ) else {
                    return None;
                };
                Some((
                    Arc::clone(&frame.volume),
                    volume_ptr,
                    frame.identity.scan_time_utc,
                    frame.identity.site_id.clone(),
                    site_lat,
                    site_lon,
                    previous_volume,
                    dealias_env,
                    context,
                ))
            });
            if let Some((
                volume,
                volume_ptr,
                scan_time,
                site_id,
                site_lat,
                site_lon,
                previous_volume,
                dealias_env,
                context,
            )) = next
            {
                let spec = self.tor_tracks.spec;
                let engine = self.dealias_engine;
                let (tx, rx) = mpsc::channel();
                self.tor_tracks.job = Some(TrackJob { rx });
                let ctx = ctx.clone();
                thread::spawn(move || {
                    let cut_indices = low_level_azshear_cut_indices(&volume);
                    let grids = crate::resolve_dealiased_cut_grids(
                        &volume,
                        previous_volume.as_ref(),
                        dealias_env.as_ref(),
                        engine,
                        &cut_indices,
                    );
                    let borrowed: Vec<Option<&radar_core::MomentGrid>> =
                        grids.iter().map(Option::as_deref).collect();
                    let grid =
                        low_level_azshear_cartesian_from_dealiased(&volume, &borrowed, &spec);
                    let sites = detect_rotation_sites_from_dealiased(&volume, &borrowed);
                    let tds = detect_tds_gates(&volume, &sites);
                    let _ = tx.send(TrackJobResult {
                        site_id,
                        site_lat,
                        site_lon,
                        scan_time,
                        volume_ptr,
                        context,
                        grid,
                        tds,
                    });
                    ctx.request_repaint();
                });
            }
        }

        // 5. Keep the displayed composite in sync with the viewed frame.
        if self.tor_tracks.show_tracks {
            self.update_tor_tracks_display(ctx);
        }
    }

    /// Scan time of the frame the user is looking at (scrub position).
    fn tor_tracks_upto(&self) -> Option<DateTime<Utc>> {
        self.primary
            .history
            .get(self.primary.cursor.index)
            .map(|frame| frame.identity.scan_time_utc)
            .or_else(|| {
                self.volume
                    .as_ref()
                    .map(|volume| volume.volume_time.with_timezone(&Utc))
            })
    }

    /// Fold the per-frame grids into the displayed running max for the viewed
    /// frame. Forward scrubs fold only the newly eligible frames; backward
    /// jumps and content changes refold from scratch.
    fn update_tor_tracks_display(&mut self, ctx: &egui::Context) {
        let Some(upto) = self.tor_tracks_upto() else {
            self.tor_tracks.display = None;
            return;
        };
        let state = &mut self.tor_tracks;
        let generation = state.generation;
        // Untouched since last update → nothing to do (the common idle path).
        if state
            .display
            .as_ref()
            .is_some_and(|display| display.generation == generation && display.upto == upto)
        {
            return;
        }
        // Forward in time with the same content → fold only the new frames.
        let incremental = state
            .display
            .as_ref()
            .is_some_and(|display| display.generation == generation && display.upto < upto);
        let (mut grid, mut frames_folded, fold_after, prior_texture) = if incremental {
            let display = state.display.take().expect("checked incremental above");
            let after = display.upto;
            (
                display.grid,
                display.frames_folded,
                Some(after),
                display.texture,
            )
        } else {
            state.display = None;
            (vec![f32::NAN; state.spec.cell_count()], 0usize, None, None)
        };
        let mut folded_any = false;
        for frame in &state.frames {
            if frame.scan_time > upto || !state.in_window(frame.scan_time) {
                continue;
            }
            if fold_after.is_some_and(|after| frame.scan_time <= after) {
                continue;
            }
            max_composite_into(&mut grid, &frame.grid);
            frames_folded += 1;
            folded_any = true;
        }
        let texture = if frames_folded == 0 {
            None
        } else if folded_any || prior_texture.is_none() {
            let size = state.spec.size();
            let pixels: Vec<egui::Color32> = grid
                .iter()
                .map(|&value| {
                    let [r, g, b, a] = rotation_track_color(value);
                    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
                })
                .collect();
            let image = egui::ColorImage {
                size: [size, size],
                source_size: egui::vec2(size as f32, size as f32),
                pixels,
            };
            Some(ctx.load_texture("tor-tracks", image, egui::TextureOptions::LINEAR))
        } else {
            prior_texture
        };
        state.display = Some(TracksDisplay {
            generation,
            upto,
            frames_folded,
            grid,
            texture,
        });
    }

    /// Map layers: tracks swath under the TDS gates. Both honor the scrub
    /// position (accumulation up to the viewed frame).
    pub(crate) fn draw_tor_tracks(&self, painter: &egui::Painter, rect: egui::Rect) {
        let state = &self.tor_tracks;
        if (!state.show_tracks && !state.show_tds)
            || state.site_id.is_none()
            || !state.site_lat.is_finite()
            || !state.site_lon.is_finite()
        {
            return;
        }
        let (site_lat, site_lon) = (state.site_lat, state.site_lon);
        let px_per_km = self.map_scale / 111.32;

        if state.show_tracks
            && let Some(display) = &state.display
            && let Some(texture) = &display.texture
            && display.frames_folded > 0
        {
            let center = self.lon_lat_to_screen(rect, site_lon, site_lat);
            let half_px = state.spec.half_extent_km * px_per_km;
            let image_rect =
                egui::Rect::from_center_size(center, egui::vec2(half_px * 2.0, half_px * 2.0));
            if image_rect.intersects(rect) {
                crate::paint_rotated_image(
                    painter,
                    texture.id(),
                    image_rect,
                    center,
                    self.aeqd_north_angle(rect, site_lat, site_lon),
                    egui::Color32::from_white_alpha(
                        (state.tracks_opacity.clamp(0.0, 1.0) * 255.0) as u8,
                    ),
                );
            }
        }

        if state.show_tds {
            let Some(upto) = self.tor_tracks_upto() else {
                return;
            };
            let newest_eligible = state
                .frames
                .iter()
                .filter(|frame| frame.scan_time <= upto && state.in_window(frame.scan_time))
                .map(|frame| frame.scan_time)
                .max();
            let trail = egui::Color32::from_rgba_unmultiplied(255, 60, 255, 150);
            let mut current_centroid = egui::Vec2::ZERO;
            let mut current_count = 0usize;
            for frame in &state.frames {
                if frame.scan_time > upto || !state.in_window(frame.scan_time) {
                    continue;
                }
                let is_current = Some(frame.scan_time) == newest_eligible;
                for gate in &frame.tds {
                    let (lat, lon) = crate::aeqd_inverse_km(
                        site_lat as f64,
                        site_lon as f64,
                        gate.east_km as f64,
                        gate.north_km as f64,
                    );
                    let position = self.lon_lat_to_screen(rect, lon as f32, lat as f32);
                    if !rect.expand(8.0).contains(position) {
                        continue;
                    }
                    if is_current {
                        // High-contrast: white core, magenta ring (debris
                        // gate at the viewed frame).
                        let radius = (0.25 * px_per_km).clamp(2.0, 5.0);
                        painter.circle_filled(position, radius, egui::Color32::WHITE);
                        painter.circle_stroke(
                            position,
                            radius + 1.0,
                            egui::Stroke::new(1.4_f32, egui::Color32::from_rgb(235, 30, 235)),
                        );
                        current_centroid += position.to_vec2();
                        current_count += 1;
                    } else {
                        // Earlier loop frames: the magenta debris track.
                        let radius = (0.15 * px_per_km).clamp(1.2, 3.0);
                        painter.circle_filled(position, radius, trail);
                    }
                }
            }
            if current_count > 0 && self.map_scale >= 80.0 {
                let centroid: egui::Pos2 = (current_centroid / current_count as f32).to_pos2();
                crate::draw_halo_text(
                    painter,
                    centroid + egui::vec2(0.0, -16.0),
                    egui::Align2::CENTER_BOTTOM,
                    "TDS flag",
                    egui::FontId::proportional(11.0),
                    egui::Color32::from_rgb(255, 235, 255),
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 210),
                );
            }
        }
    }

    /// Rotation tracks + TDS as unified layer-rail rows (ui-overhaul spec
    /// §2: every map overlay is a row). Reset lives behind the row's ⚙
    /// (wave 2: the inline 50 pt button outgrew the middle zone at 320 pt).
    pub(crate) fn tor_tracks_rail_rows(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        use crate::{LayerRowGear, LayerRowOpacity, LayerRowSpec, LayerRowVis, layer_row};
        let previous_show_tracks = self.tor_tracks.show_tracks;
        let previous_show_tds = self.tor_tracks.show_tds;
        let newest = self
            .primary
            .history
            .last()
            .map(|frame| frame.identity.scan_time_utc);
        let state = &mut self.tor_tracks;
        let mut reset = false;
        let can_reset = (state.show_tracks || state.show_tds) && !state.frames.is_empty();
        if layer_row(
            ui,
            LayerRowSpec {
                vis: LayerRowVis::Toggle {
                    value: &mut state.show_tracks,
                    hover: "Per-pixel MAXIMUM low-level (0–2 km, lowest tilts) cyclonic azimuthal shear accumulated across the loaded loop — the swath a translating mesocyclone paints. Single-radar analogue of the MRMS rotation tracks (Mahalik et al. 2019; Miller et al. 2013; Smith et al. 2016). Transparent below 0.003 s⁻¹, magenta at 0.02 s⁻¹. Scrubbing shows the accumulation up to the viewed frame.",
                },
                name: "Rotation tracks",
                name_hover: "Low-level azimuthal-shear swath across the loop (MRMS rotation-tracks lineage)",
                opacity: Some(LayerRowOpacity::F32 {
                    value: &mut state.tracks_opacity,
                    min: 0.2,
                    hover: "Rotation-tracks layer opacity",
                }),
                gear: Some(LayerRowGear::Menu {
                    hover: "Rotation-tracks options (reset window)",
                    content: Box::new(|ui| {
                        if ui
                            .add_enabled(can_reset, egui::Button::new("Reset accumulation"))
                            .on_hover_text(
                                "Restart the accumulation window at the newest loaded frame",
                            )
                            .clicked()
                        {
                            reset = true;
                            ui.close();
                        }
                        ui.separator();
                        ui.weak("Appearance controls (ramp, thresholds)");
                        ui.weak("land here next.");
                    }),
                }),
                ..Default::default()
            },
            |_ui| {},
        ) {
            ctx.request_repaint();
        }
        if reset {
            state.reset_floor = newest;
            if let Some(floor) = state.reset_floor {
                state
                    .frames
                    .retain(|frame| !frame.persisted && frame.scan_time >= floor);
            }
            state.display = None;
            state.generation = state.generation.wrapping_add(1);
            ctx.request_repaint();
        }
        if layer_row(
            ui,
            LayerRowSpec {
                vis: LayerRowVis::Toggle {
                    value: &mut state.show_tds,
                    hover: "Tornado debris signature — a deterministic dual-pol physics flag, NOT a probability: ρhv < 0.82 inside > 30 dBZ echo within 5 km of a rank ≥ 3 circulation, lowest tilt (Ryzhkov et al. 2005; Van Den Broeke & Jauernic 2014; Snyder & Ryzhkov 2015). White/magenta gates at the viewed frame; the magenta trail is the debris track across the loop.",
                },
                name: "TDS flag",
                name_hover: "Dual-pol tornado debris signature gates + the debris track across the loop",
                gear: Some(LayerRowGear::Menu {
                    hover: "TDS options",
                    content: Box::new(|ui| {
                        ui.weak("Appearance controls land here next.");
                    }),
                }),
                ..Default::default()
            },
            |_ui| {},
        ) {
            ctx.request_repaint();
        }
        if state.show_tracks || state.show_tds {
            let processed = state.frames.len();
            let pending = state.job.is_some();
            let label = match (processed, pending) {
                (0, true) => "tracks: processing…".to_owned(),
                (0, false) => "tracks: no frames yet".to_owned(),
                (n, pending) => {
                    let first = state.frames.first().map(|f| f.scan_time);
                    let last = state.frames.last().map(|f| f.scan_time);
                    let window = match (first, last) {
                        (Some(a), Some(b)) => {
                            format!(" · {}–{}Z", a.format("%H%M"), b.format("%H%M"))
                        }
                        _ => String::new(),
                    };
                    format!(
                        "tracks: {n} frame{}{}{}",
                        if n == 1 { "" } else { "s" },
                        window,
                        if pending { " · processing…" } else { "" }
                    )
                }
            };
            crate::panel_kit::status_block(ui, &label, None);
        }
        let show_tracks = state.show_tracks;
        let show_tds = state.show_tds;
        if previous_show_tracks != show_tracks || previous_show_tds != show_tds {
            self.set_tor_track_layers_visible(show_tracks, show_tds);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_rotation_accumulation_binary_round_trips() {
        let spec = TracksGridSpec {
            half_extent_km: 1.0,
            cell_km: 0.5,
        };
        let cache = RotationTrackCache {
            site_id: "KTLX".to_owned(),
            site_lat: 35.333,
            site_lon: -97.278,
            spec,
            upto: DateTime::from_timestamp_millis(1_750_000_123_456).unwrap(),
            grid: (0..spec.cell_count())
                .map(|index| {
                    if index % 3 == 0 {
                        f32::NAN
                    } else {
                        index as f32
                    }
                })
                .collect(),
        };

        let bytes = encode_rotation_track_cache(&cache).expect("encode cache");
        let decoded = decode_rotation_track_cache(&bytes).expect("decode cache");

        assert_eq!(decoded.site_id, cache.site_id);
        assert_eq!(decoded.site_lat.to_bits(), cache.site_lat.to_bits());
        assert_eq!(decoded.site_lon.to_bits(), cache.site_lon.to_bits());
        assert_eq!(decoded.spec, cache.spec);
        assert_eq!(decoded.upto, cache.upto);
        assert_eq!(decoded.grid.len(), cache.grid.len());
        for (actual, expected) in decoded.grid.iter().zip(&cache.grid) {
            assert!(
                (actual.is_nan() && expected.is_nan()) || actual.to_bits() == expected.to_bits()
            );
        }
    }

    #[test]
    fn persisted_rotation_accumulation_rejects_truncation() {
        let spec = TracksGridSpec {
            half_extent_km: 1.0,
            cell_km: 1.0,
        };
        let cache = RotationTrackCache {
            site_id: "KOUN".to_owned(),
            site_lat: 35.2,
            site_lon: -97.4,
            spec,
            upto: Utc::now(),
            grid: vec![0.01; spec.cell_count()],
        };
        let mut bytes = encode_rotation_track_cache(&cache).unwrap();
        bytes.pop();
        assert!(decode_rotation_track_cache(&bytes).is_none());
    }
}
