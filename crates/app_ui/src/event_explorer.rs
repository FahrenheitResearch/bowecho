//! EVENT EXPLORER: pick a convective day, see everything that happened —
//! every storm report, clickable tornado tracks, the day's outlook — and
//! click into the radar loop "like it would have been live".
//!
//! Composition over invention: the reports/outlook fetch and drawing live
//! in `spc_layers` (now date-aware), the click-to-radar jump is the
//! `jump_to_spc_report` flow generalized to a track's begin→end window,
//! and the second radar rides the existing overlay-layer machinery with
//! an archive volume instead of the live one.
//!
//! Day selection: an explicit pin from the DATA-tab "Event day" row wins;
//! otherwise the layer FOLLOWS the displayed volume's convective day
//! (12Z→12Z, [`spc_layers::spc_convective_date`]) the same way the SPC
//! outlooks follow the displayed day. The current convective day keeps
//! the live `today_filtered` path untouched.
//!
//! Data sources (cited in `spc_layers`): SPC climo filtered/raw report
//! CSVs (per convective day, 2004+) and the SPC WCM tornado database
//! per-year files (begin/end track coordinates; Schaefer & Edwards 1999).

use crate::spc_layers::{self, EventDayData};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use eframe::egui;
use std::collections::{BTreeSet, HashMap};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Click/hover tolerance around a track segment, screen px.
const EVENT_TRACK_CLICK_PX: f32 = 8.0;
/// Same eligibility radius as the report jump (lowest-beam rule).
const EVENT_RADAR_MAX_RANGE_KM: f32 = 460.0;
/// Track duration estimate: the WCM database carries no end TIME, only a
/// path length, so assume the climatological ~30 mph translation speed
/// (path-length/duration scaling per Brooks 2004, Wea. Forecasting 19,
/// 310-319, doi:10.1175/1520-0434(2004)019<0310:OTROTP>2.0.CO;2).
const TRACK_TRANSLATION_MPH: f32 = 30.0;
const TRACK_MAX_DURATION_MINUTES: i64 = 120;
/// A failed (transport, not 404) day fetch waits this long before an
/// automatic retry — "never an error spam loop".
const EVENT_FETCH_RETRY_SECONDS: u64 = 60;

#[derive(Default)]
pub(crate) struct EventExplorerState {
    /// DATA-tab date field (YYYY-MM-DD), archive-input conventions.
    pub date_input: String,
    /// Explicit day pin from the panel; `None` follows the displayed
    /// volume's convective day.
    pub pinned_day: Option<NaiveDate>,
    /// Per-convective-day cache — a day is fetched once per session
    /// (404 days cache as `reports_file_missing`, so quiet days never
    /// refetch either).
    pub cache: HashMap<NaiveDate, EventDayData>,
    fetch: Option<(NaiveDate, mpsc::Receiver<Result<EventDayData, String>>)>,
    /// Last transport failure, rate-limiting retries.
    failed: Option<(NaiveDate, Instant)>,
    /// Track click armed an archive-listing load of this UTC window;
    /// consumed when the listing lands.
    pub pending_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// One-shot: start the history loop when the range load installs.
    pub pending_autoplay: bool,
}

impl EventExplorerState {
    pub(crate) fn fetching_day(&self) -> Option<NaiveDate> {
        self.fetch.as_ref().map(|(day, _)| *day)
    }

    #[cfg(test)]
    pub(crate) fn set_fetch_for_test(
        &mut self,
        day: NaiveDate,
        receiver: mpsc::Receiver<Result<EventDayData, String>>,
    ) {
        self.fetch = Some((day, receiver));
    }
}

/// What a click on a tornado track resolves to (lat/lon pairs).
#[derive(Clone, Debug)]
pub(crate) struct EventTrackHit {
    pub begin: (f32, f32),
    pub end: (f32, f32),
    pub time_utc: DateTime<Utc>,
    /// Surveyed lift time when the database carries one.
    pub end_time_utc: Option<DateTime<Utc>>,
    pub length_mi: f32,
    pub label: String,
}

impl EventTrackHit {
    /// When the tornado lifted: the surveyed end time where the database
    /// carries one, else the path-length estimate
    /// ([`estimated_track_end_time`]).
    fn lift_time(&self) -> DateTime<Utc> {
        self.end_time_utc
            .unwrap_or_else(|| estimated_track_end_time(self.time_utc, self.length_mi))
    }
}

/// Distance from `point` to the segment `a`-`b` in screen px (the
/// standard clamped projection; zero-length segments degrade to point
/// distance).
pub(crate) fn point_segment_distance(point: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
    let ab = b - a;
    let length_sq = ab.length_sq();
    if length_sq <= f32::EPSILON {
        return point.distance(a);
    }
    let t = ((point - a).dot(ab) / length_sq).clamp(0.0, 1.0);
    point.distance(a + ab * t)
}

/// Dual-radar pick for a track, pure over a site list of
/// `(catalog_index, lat, lon)`: PRIMARY = nearest site to the track
/// MIDPOINT (within the lowest-beam radius), OVERLAY = nearest site to
/// the track END when it differs from the primary — the owner's
/// "selecting two radars if a track is loaded".
pub(crate) fn select_event_radar_indices(
    sites: &[(usize, f32, f32)],
    begin: (f32, f32),
    end: (f32, f32),
) -> Option<(usize, Option<usize>)> {
    let midpoint = ((begin.0 + end.0) / 2.0, (begin.1 + end.1) / 2.0);
    let nearest_to = |target: (f32, f32)| {
        sites
            .iter()
            .filter_map(|&(index, lat, lon)| {
                let distance_km = crate::haversine_km(target.0, target.1, lat, lon);
                (distance_km <= EVENT_RADAR_MAX_RANGE_KM).then_some((index, distance_km))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(index, _)| index)
    };
    let primary = nearest_to(midpoint)?;
    let overlay = nearest_to(end).filter(|&index| index != primary);
    Some((primary, overlay))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EventTrackLoadPlan {
    load_primary_archive: bool,
    overlay_index: Option<usize>,
}

fn event_track_load_plan(
    primary_cache_hit: bool,
    overlay_index: Option<usize>,
) -> EventTrackLoadPlan {
    EventTrackLoadPlan {
        load_primary_archive: !primary_cache_hit,
        overlay_index,
    }
}

/// Estimated time the tornado lifted: begin time plus path length over
/// the ~30 mph climatological translation speed (see
/// [`TRACK_TRANSLATION_MPH`]), clamped to two hours.
pub(crate) fn estimated_track_end_time(begin: DateTime<Utc>, length_mi: f32) -> DateTime<Utc> {
    let minutes = if length_mi > 0.0 {
        ((length_mi / TRACK_TRANSLATION_MPH * 60.0) as i64).clamp(0, TRACK_MAX_DURATION_MINUTES)
    } else {
        0
    };
    begin + ChronoDuration::minutes(minutes)
}

/// "HH:MM:SS" display/sort label for an archive object key
/// (KXXX20260609_235423_V06 -> "23:54:23"), the same convention the
/// archive listing builds — labels sort chronologically within a date.
pub(crate) fn volume_time_label(key: &str) -> String {
    key.rsplit('/')
        .next()
        .and_then(|name| name.split('_').nth(1))
        .filter(|t| t.len() == 6 && t.bytes().all(|b| b.is_ascii_digit()))
        .map(|t| format!("{}:{}:{}", &t[0..2], &t[2..4], &t[4..6]))
        .unwrap_or_else(|| "??".to_owned())
}

/// Index of the volume scanning at `target_label` — the LAST volume at
/// or before it, or the first when the target precedes the day's first
/// scan (the `archive_pending_event` rule, extracted).
pub(crate) fn nearest_volume_index(labels: &[String], target_label: &str) -> Option<usize> {
    if labels.is_empty() {
        return None;
    }
    Some(
        labels
            .iter()
            .position(|label| label.as_str() > target_label)
            .unwrap_or(labels.len())
            .saturating_sub(1),
    )
}

/// The volume range covering a [start, end] window: from the volume
/// scanning at the window start through the last volume inside it, plus
/// `pad` scans of context each side (clamped to the day's listing — the
/// user-set frames-before-touchdown/after-lift, default 5), capped at
/// `cap` frames by advancing the start (keep the track's end — matches
/// how the frame history trims oldest-first).
pub(crate) fn range_volume_indices(
    labels: &[String],
    start_label: &str,
    end_label: &str,
    pad: usize,
    cap: usize,
) -> Option<(usize, usize)> {
    if cap == 0 {
        return None;
    }
    let end = nearest_volume_index(labels, end_label)?;
    let start = nearest_volume_index(labels, start_label)?.min(end);
    let mut start = start.saturating_sub(pad);
    let end = (end + pad).min(labels.len().saturating_sub(1));
    if end + 1 - start > cap {
        start = end + 1 - cap;
    }
    Some((start, end))
}

/// Clamp a UTC window to one listed archive date's "HH:MM:SS" label
/// space (the listing is per-date; a window edge on another calendar day
/// clamps to that date's boundary).
pub(crate) fn window_labels_for_date(
    listed: NaiveDate,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> (String, String) {
    let start_label = if start.date_naive() < listed {
        "00:00:00".to_owned()
    } else {
        start.format("%H:%M:%S").to_string()
    };
    let end_label = if end.date_naive() > listed {
        "23:59:59".to_owned()
    } else {
        end.format("%H:%M:%S").to_string()
    };
    (start_label, end_label)
}

impl crate::ViewerApp {
    /// The convective day the reports layer should show, or `None` for
    /// the live path: the panel pin wins, else FOLLOW the displayed
    /// volume's day (exactly how the outlooks follow archive browsing).
    /// The current convective day maps to `None` — live reports.
    pub(crate) fn event_followed_day(&self) -> Option<NaiveDate> {
        let followed = self.event_explorer.pinned_day.or_else(|| {
            self.volume.as_ref().map(|volume| {
                spc_layers::spc_convective_date(volume.volume_time.with_timezone(&Utc))
            })
        })?;
        (followed != spc_layers::spc_convective_date(Utc::now())).then_some(followed)
    }

    /// The followed day's cached data, when fetched.
    pub(crate) fn active_event_day_data(&self) -> Option<&EventDayData> {
        self.event_explorer.cache.get(&self.event_followed_day()?)
    }

    /// Reports the map should draw right now: the followed day's cached
    /// set, nothing while that day is still fetching (live reports for
    /// another day would mislead), or the live set.
    pub(crate) fn reports_for_display(&self) -> &[spc_layers::StormReport] {
        match self.event_followed_day() {
            Some(day) => self
                .event_explorer
                .cache
                .get(&day)
                .map(|data| data.reports.as_slice())
                .unwrap_or(&[]),
            None => &self.spc_data.reports,
        }
    }

    /// Per-update pump: install a finished day fetch, kick the next one
    /// when the followed day is not cached. Reports off = fully idle.
    pub(crate) fn poll_event_day(&mut self, ctx: &egui::Context) {
        if let Some((day, receiver)) = &self.event_explorer.fetch {
            let day = *day;
            match receiver.try_recv() {
                Ok(Ok(data)) => {
                    self.event_explorer.fetch = None;
                    self.status = if data.reports_file_missing && data.segments.is_empty() {
                        format!("SPC {day}: no reports file for this day")
                    } else {
                        format!(
                            "SPC {day}: {} reports · {} tornado segments",
                            data.reports.len(),
                            data.segments.len()
                        )
                    };
                    self.event_explorer.cache.insert(day, data);
                    ctx.request_repaint();
                }
                Ok(Err(err)) => {
                    self.event_explorer.fetch = None;
                    self.event_explorer.failed = Some((day, Instant::now()));
                    self.status = format!("SPC {day} fetch failed: {err}");
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.event_explorer.fetch = None,
            }
        }
        if !self.spc_reports_enabled || self.event_explorer.fetch.is_some() {
            return;
        }
        let Some(day) = self.event_followed_day() else {
            return;
        };
        if self.event_explorer.cache.contains_key(&day) {
            return;
        }
        if self.event_explorer.failed.is_some_and(|(failed_day, at)| {
            failed_day == day && at.elapsed() < Duration::from_secs(EVENT_FETCH_RETRY_SECONDS)
        }) {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.event_explorer.fetch = Some((day, receiver));
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = sender.send(spc_layers::fetch_event_day(day));
            ctx.request_repaint();
        });
    }

    /// DATA-tab "Event day" row: a date field + Load that pins the
    /// reports/outlook day (the map stays put — no zoom, no site change)
    /// and a count line. Nothing here persists; the layer toggles
    /// already do.
    pub(crate) fn event_day_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.event_explorer.date_input.is_empty() {
            self.event_explorer.date_input = spc_layers::spc_convective_date(Utc::now())
                .format("%Y-%m-%d")
                .to_string();
        }
        let mut load: Option<NaiveDate> = None;
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.event_explorer.date_input)
                    .hint_text("YYYY-MM-DD")
                    .desired_width(88.0),
            )
            .on_hover_text(
                "SPC convective day (12Z-12Z): a 03Z report belongs to the PREVIOUS day's file",
            );
            if ui
                .button("Load")
                .on_hover_text(
                    "Show this day's storm reports, tornado tracks, and outlook. The map stays put — click a track to load its radar loop.",
                )
                .clicked()
            {
                match NaiveDate::parse_from_str(self.event_explorer.date_input.trim(), "%Y-%m-%d")
                {
                    Ok(date) => load = Some(date),
                    Err(_) => self.status = "Event day must be YYYY-MM-DD".to_owned(),
                }
            }
            if self.event_explorer.pinned_day.is_some()
                && ui
                    .button("Unpin")
                    .on_hover_text(
                        "Back to following the displayed radar time's day (live when current)",
                    )
                    .clicked()
            {
                self.event_explorer.pinned_day = None;
                self.spc_data.fetched_at = None; // outlook follows again
                ctx.request_repaint();
            }
            // Track-click loop context (persisted): scans loaded beyond
            // the track window on each side — short tracks otherwise
            // land a loop of only a few frames (field request).
            let mut pad = crate::normalized_event_pad_frames(self.app_settings.event_pad_frames);
            ui.label("Track pad").on_hover_text(
                "Tornado track clicks only. SPC report dots use the Archive Fetch N scans control.",
            );
            ui.add(
                egui::DragValue::new(&mut pad)
                    .range(0..=crate::MAX_EVENT_PAD_FRAMES)
                    .speed(0.1)
                    .prefix("±")
                    .suffix(" scans"),
            )
            .on_hover_text(
                "Clicking a tornado track loads this many extra archive scans before \
                 touchdown and after lift. SPC report dots use Archive > Fetch N scans instead.",
            );
            if pad != self.app_settings.event_pad_frames {
                self.app_settings.event_pad_frames = pad;
                let _ = self.app_settings.save();
            }
            if self.event_explorer.fetch.is_some() {
                ui.spinner();
            }
        });
        if let Some(date) = load {
            self.event_explorer.pinned_day = Some(date);
            self.event_explorer.failed = None;
            // The whole point is seeing the day: light the reports layer
            // (session-only — persisted defaults stay the user's).
            self.spc_reports_enabled = true;
            self.spc_data.fetched_at = None; // outlook keys onto the new day
            // Align the archive browser so a List shows the same day.
            self.archive_date_input = date.format("%Y-%m-%d").to_string();
            self.status = format!("Event day {date}");
            ctx.request_repaint();
        }

        // Count line for the active day.
        match self.event_followed_day() {
            None => {
                ui.weak("live day — today's filtered reports");
            }
            Some(day) => {
                let source = if self.event_explorer.pinned_day.is_some() {
                    "pinned"
                } else {
                    "following displayed time"
                };
                match self.event_explorer.cache.get(&day) {
                    Some(data) if data.reports_file_missing && data.segments.is_empty() => {
                        ui.weak(format!("{day} ({source}): no reports file for this day"));
                    }
                    Some(data) => {
                        let tracks = data.segments.iter().filter(|s| s.is_track()).count();
                        ui.weak(format!(
                            "{day} ({source}): {} reports · {} tornado segments ({tracks} tracks)",
                            data.reports.len(),
                            data.segments.len()
                        ));
                    }
                    None if self
                        .event_explorer
                        .fetch
                        .as_ref()
                        .is_some_and(|(d, _)| *d == day) =>
                    {
                        ui.weak(format!("{day}: fetching reports…"));
                    }
                    None if self
                        .event_explorer
                        .failed
                        .is_some_and(|(failed_day, _)| failed_day == day) =>
                    {
                        ui.weak(format!("{day}: fetch failed — Load retries"));
                    }
                    None => {
                        ui.weak(format!("{day}: waiting for reports layer"));
                    }
                }
            }
        }
    }

    /// Map layer: tornado TRACK LINES (begin→end, red, arrowhead
    /// direction cue) for the followed day, under the report dots.
    /// Zero-length segments are the report dot itself — they only draw
    /// here when the day has no report file (pre-2004 WCM-only days).
    pub(crate) fn draw_event_tracks(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !self.spc_reports_enabled {
            return;
        }
        let Some(data) = self.active_event_day_data() else {
            return;
        };
        let track_color = egui::Color32::from_rgb(235, 51, 35);
        let halo = egui::Color32::from_rgba_unmultiplied(40, 0, 0, 170);
        let marker = self.style_registry.report_marker("tornado");
        let marker_color = crate::style_color32(marker.color);
        let outline = egui::Stroke::new(
            marker.outline_width,
            crate::style_color32(marker.outline_color),
        );
        let mut hovered: Vec<String> = Vec::new();
        let pointer = painter
            .ctx()
            .pointer_hover_pos()
            .filter(|pos| rect.contains(*pos));
        let cull = rect.expand(400.0);
        for segment in &data.segments {
            let a = self.lon_lat_to_screen(rect, segment.begin_lon, segment.begin_lat);
            match segment.end {
                Some((end_lat, end_lon)) => {
                    let b = self.lon_lat_to_screen(rect, end_lon, end_lat);
                    if !cull.contains(a) && !cull.contains(b) {
                        continue;
                    }
                    painter.line_segment([a, b], egui::Stroke::new(4.0, halo));
                    painter.line_segment([a, b], egui::Stroke::new(2.2, track_color));
                    // Direction cue: a small arrowhead at the END point
                    // (only once the track is long enough to read).
                    let delta = b - a;
                    if delta.length() >= 14.0 {
                        let along = delta.normalized();
                        let left = egui::vec2(-along.y, along.x);
                        painter.add(egui::Shape::convex_polygon(
                            vec![
                                b,
                                b - along * 7.0 + left * 3.5,
                                b - along * 7.0 - left * 3.5,
                            ],
                            track_color,
                            egui::Stroke::NONE,
                        ));
                    }
                    if let Some(pointer) = pointer
                        && point_segment_distance(pointer, a, b) <= EVENT_TRACK_CLICK_PX
                        && hovered.len() < 5
                    {
                        hovered.push(segment.hover_text());
                    }
                }
                None => {
                    // The torn report dot already marks this point unless
                    // the day predates the report archive.
                    if !data.reports_file_missing || !rect.contains(a) {
                        continue;
                    }
                    let scale = marker.size_px / 5.0;
                    painter.add(egui::Shape::convex_polygon(
                        vec![
                            a + egui::vec2(0.0, -5.0 * scale),
                            a + egui::vec2(4.5 * scale, 3.5 * scale),
                            a + egui::vec2(-4.5 * scale, 3.5 * scale),
                        ],
                        marker_color,
                        outline,
                    ));
                    if let Some(pointer) = pointer
                        && pointer.distance(a) <= EVENT_TRACK_CLICK_PX
                        && hovered.len() < 5
                    {
                        hovered.push(segment.hover_text());
                    }
                }
            }
        }
        if let Some(pointer) = pointer
            && !hovered.is_empty()
        {
            painter
                .ctx()
                .set_cursor_icon(egui::CursorIcon::PointingHand);
            // The report-dot tooltip wins where both would fire (stacked
            // popups at one anchor are unreadable) — the dots carry the
            // same story for coincident points.
            let dot_nearby = self.reports_for_display().iter().any(|report| {
                self.lon_lat_to_screen(rect, report.lon, report.lat)
                    .distance(pointer)
                    <= 10.0
            });
            if !dot_nearby {
                egui::Tooltip::always_open(
                    painter.ctx().clone(),
                    egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("event_trk_hover")),
                    egui::Id::new("event_trk_tip"),
                    egui::PopupAnchor::Pointer,
                )
                .gap(12.0)
                .show(|ui| {
                    ui.set_max_width(340.0);
                    ui.label(hovered.join("\n――――――\n"));
                });
            }
        }
    }

    /// Hit-test a click against the followed day's tornado tracks:
    /// point-to-segment distance for surveyed tracks, point distance for
    /// zero-length segments AND torn report dots (so recent days without
    /// WCM coverage stay clickable). Nearest within the 8 px halo wins.
    pub(crate) fn event_track_at(
        &self,
        rect: egui::Rect,
        pointer: egui::Pos2,
    ) -> Option<EventTrackHit> {
        if !self.spc_reports_enabled {
            return None;
        }
        let data = self.active_event_day_data()?;
        let mut best: Option<(f32, EventTrackHit)> = None;
        let mut consider = |distance: f32, hit: EventTrackHit| {
            if distance <= EVENT_TRACK_CLICK_PX && best.as_ref().is_none_or(|(d, _)| distance < *d)
            {
                best = Some((distance, hit));
            }
        };
        for segment in &data.segments {
            let a = self.lon_lat_to_screen(rect, segment.begin_lon, segment.begin_lat);
            let distance = match segment.end {
                Some((end_lat, end_lon)) => {
                    let b = self.lon_lat_to_screen(rect, end_lon, end_lat);
                    point_segment_distance(pointer, a, b)
                }
                None => pointer.distance(a),
            };
            consider(
                distance,
                EventTrackHit {
                    begin: (segment.begin_lat, segment.begin_lon),
                    end: segment.end_or_begin(),
                    time_utc: segment.time_utc,
                    end_time_utc: segment.end_time_utc,
                    length_mi: segment.length_mi,
                    label: format!("{} {}", segment.ef_label, segment.location),
                },
            );
        }
        // Torn report dots double as zero-length events (consistent
        // click behavior whether or not the WCM database covers the day).
        for report in self.reports_for_display() {
            if report.kind != spc_layers::ReportKind::Tornado {
                continue;
            }
            let position = self.lon_lat_to_screen(rect, report.lon, report.lat);
            consider(
                pointer.distance(position),
                EventTrackHit {
                    begin: (report.lat, report.lon),
                    end: (report.lat, report.lon),
                    time_utc: report.time_utc,
                    end_time_utc: None,
                    length_mi: 0.0,
                    label: report.location.clone(),
                },
            );
        }
        best.map(|(_, hit)| hit)
    }

    /// Track click: PRIMARY = the lowest-beam WSR-88D nearest the track
    /// midpoint, loaded as an archive loop spanning the track window plus
    /// `event_pad_frames` scans of context each side that auto-plays at
    /// the lowest tilt; when the radar nearest the track END differs, it
    /// loads as a second radar overlay at the event time. (The
    /// `jump_to_spc_report` flow, generalized.)
    pub(crate) fn jump_to_event_track(&mut self, hit: &EventTrackHit, ctx: &egui::Context) {
        let eligible: Vec<(usize, f32, f32)> = self
            .sites
            .iter()
            .enumerate()
            .filter_map(|(index, site)| {
                // WSR-88Ds only — TDWRs' short range / attenuation make
                // them the wrong default for an event jump.
                if site.level2_id.starts_with('T') {
                    return None;
                }
                let (lat, lon) = crate::site_location(site)?;
                Some((index, lat, lon))
            })
            .collect();
        let Some((primary, overlay)) = select_event_radar_indices(&eligible, hit.begin, hit.end)
        else {
            self.status = "No radar within 460 km of that track".to_owned();
            return;
        };
        self.selected_site_index = primary;
        let primary_site_id = self.sites[primary].level2_id.clone();
        self.map_center_lat = (hit.begin.0 + hit.end.0) / 2.0;
        self.map_center_lon = (hit.begin.1 + hit.end.1) / 2.0;
        self.map_scale = self.map_scale.max(220.0);
        let end_time = hit.lift_time();
        let primary_cache_hit =
            self.select_cached_event_frame(&primary_site_id, hit.time_utc, &hit.label, ctx);
        let load_plan = event_track_load_plan(primary_cache_hit, overlay);
        if load_plan.load_primary_archive {
            // The track time's RADAR date can differ from the SPC file date
            // (12Z convention) — list the track's own calendar date.
            self.archive_date_input = hit.time_utc.format("%Y-%m-%d").to_string();
            self.event_explorer.pending_range = Some((hit.time_utc, end_time));
            self.status = format!(
                "Event jump: {} · loop {}–{}Z ±{} scans",
                hit.label,
                hit.time_utc.format("%H%M"),
                end_time.format("%H%M"),
                crate::normalized_event_pad_frames(self.app_settings.event_pad_frames),
            );
            self.start_archive_listing(ctx);
        }
        if let Some(overlay_index) = load_plan.overlay_index
            && let Some(site) = self.sites.get(overlay_index).cloned()
        {
            self.start_radar_layer_event_load(site, end_time, ctx);
        }
    }

    /// Load ONE archive volume nearest `target` into a radar overlay
    /// layer (the second radar of a track jump) — the overlay-layer
    /// machinery with an archive object instead of the live latest.
    pub(crate) fn start_radar_layer_event_load(
        &mut self,
        site: data_source::RadarSite,
        target: DateTime<Utc>,
        ctx: &egui::Context,
    ) {
        let index = match self
            .radar_layers
            .iter()
            .position(|layer| layer.site.level2_id == site.level2_id)
        {
            Some(index) => index,
            None => {
                if self.radar_layers.len() >= crate::MAX_RADAR_OVERLAY_LAYERS {
                    let remove_index = self
                        .radar_layers
                        .iter()
                        .position(|layer| !layer.visible)
                        .unwrap_or(0);
                    self.radar_layers.remove(remove_index);
                }
                let id = self.next_radar_layer_id;
                self.next_radar_layer_id = self.next_radar_layer_id.saturating_add(1);
                self.radar_layers
                    .push(crate::RadarOverlayLayer::new(id, site.clone()));
                self.radar_layers.len() - 1
            }
        };
        let layer = &mut self.radar_layers[index];
        layer.visible = true;
        if layer.load_receiver.is_some() {
            return;
        }
        let site_id = layer.site.level2_id.clone();
        let (sender, receiver) = mpsc::channel();
        layer.load_receiver = Some(receiver);
        layer.status = format!("Loading {site_id} @ {}Z", target.format("%H%M"));
        self.status = format!("Second radar {site_id} @ {}Z", target.format("%H%M"));
        let site_cache = crate::cache_dir(&site_id);
        let worker_ctx = ctx.clone();
        thread::spawn(move || {
            let label = format!("L2 {site_id} event overlay");
            let send_final = |result: Result<crate::DecodedLoadBatch, String>| {
                let _ = sender.send(crate::AsyncLoadResult {
                    label: label.clone(),
                    update: crate::AsyncLoadUpdate::Final(result),
                });
                worker_ctx.request_repaint();
            };
            let objects = match data_source::level2_objects_for_date(&site_id, target.date_naive())
            {
                Ok(objects) => objects,
                Err(err) => {
                    send_final(Err(err.to_string()));
                    return;
                }
            };
            let labels: Vec<String> = objects
                .iter()
                .map(|object| volume_time_label(&object.key))
                .collect();
            let target_label = target.format("%H:%M:%S").to_string();
            let Some(nearest) = nearest_volume_index(&labels, &target_label) else {
                send_final(Err(format!(
                    "no archive volumes for {site_id} on {}",
                    target.date_naive()
                )));
                return;
            };
            match crate::decode_archive_history_object(
                &site_id,
                objects[nearest].clone(),
                &site_cache,
                &BTreeSet::new(),
                None,
                Instant::now(),
                &sender,
                false,
                true,
            ) {
                Ok(Some(decoded)) => send_final(Ok(crate::DecodedLoadBatch::single(decoded))),
                Ok(None) => send_final(Err("event volume had no displayable products".to_owned())),
                Err(err) => send_final(Err(err)),
            }
        });
        ctx.request_repaint_after(Duration::from_millis(crate::ACTIVE_LOAD_POLL_MS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn pos(x: f32, y: f32) -> egui::Pos2 {
        egui::pos2(x, y)
    }

    #[test]
    fn point_segment_distance_covers_interior_endpoints_and_degenerate() {
        let a = pos(0.0, 0.0);
        let b = pos(10.0, 0.0);
        // Perpendicular from the interior.
        assert_eq!(point_segment_distance(pos(5.0, 3.0), a, b), 3.0);
        // Beyond the ends clamps to the endpoints.
        assert_eq!(point_segment_distance(pos(-4.0, 0.0), a, b), 4.0);
        assert_eq!(point_segment_distance(pos(13.0, 4.0), a, b), 5.0);
        // Zero-length = point distance.
        assert_eq!(point_segment_distance(pos(3.0, 4.0), a, a), 5.0);
    }

    #[test]
    fn dual_radar_pick_prefers_midpoint_then_adds_the_end_site() {
        // A long track: site 0 clearly nearest the midpoint, site 1
        // clearly nearest the end -> two radars.
        let sites = [(0usize, 35.0, -98.0), (1usize, 35.0, -95.0)];
        let begin = (35.0, -98.5);
        let end = (35.0, -95.5);
        let (primary, overlay) =
            select_event_radar_indices(&sites, begin, end).expect("both in range");
        assert_eq!(primary, 0); // midpoint -97.0 is nearer site 0
        assert_eq!(overlay, Some(1)); // end -95.5 is nearer site 1

        // Zero-length track near site 0: one radar, no overlay (the
        // end's nearest IS the primary).
        let (primary, overlay) =
            select_event_radar_indices(&sites, begin, begin).expect("in range");
        assert_eq!(primary, 0);
        assert_eq!(overlay, None);

        // Out of the 460 km lowest-beam radius entirely.
        assert_eq!(
            select_event_radar_indices(&sites, (60.0, -150.0), (60.0, -150.0)),
            None
        );
    }

    #[test]
    fn event_track_load_plan_keeps_overlay_when_primary_is_cached() {
        assert_eq!(
            event_track_load_plan(true, Some(7)),
            EventTrackLoadPlan {
                load_primary_archive: false,
                overlay_index: Some(7),
            }
        );
        assert_eq!(
            event_track_load_plan(true, None),
            EventTrackLoadPlan {
                load_primary_archive: false,
                overlay_index: None,
            }
        );
    }

    #[test]
    fn event_track_load_plan_lists_primary_when_cache_misses() {
        assert_eq!(
            event_track_load_plan(false, Some(3)),
            EventTrackLoadPlan {
                load_primary_archive: true,
                overlay_index: Some(3),
            }
        );
    }

    #[test]
    fn track_end_time_scales_with_path_length_and_clamps() {
        let begin = Utc.with_ymd_and_hms(2011, 4, 27, 20, 5, 0).unwrap();
        // Zero-length: lifts immediately.
        assert_eq!(estimated_track_end_time(begin, 0.0), begin);
        // 30 mi at ~30 mph = one hour.
        assert_eq!(
            estimated_track_end_time(begin, 30.0),
            begin + ChronoDuration::minutes(60)
        );
        // Absurd lengths clamp at two hours.
        assert_eq!(
            estimated_track_end_time(begin, 400.0),
            begin + ChronoDuration::minutes(120)
        );
    }

    fn labels(times: &[&str]) -> Vec<String> {
        times.iter().map(|t| (*t).to_owned()).collect()
    }

    #[test]
    fn nearest_volume_index_picks_the_scan_covering_the_target() {
        let list = labels(&["20:00:10", "20:05:40", "20:11:05"]);
        // Exactly at, between, before-first, after-last.
        assert_eq!(nearest_volume_index(&list, "20:05:40"), Some(1));
        assert_eq!(nearest_volume_index(&list, "20:08:00"), Some(1));
        assert_eq!(nearest_volume_index(&list, "19:00:00"), Some(0));
        assert_eq!(nearest_volume_index(&list, "23:59:59"), Some(2));
        assert_eq!(nearest_volume_index(&[], "20:00:00"), None);
    }

    #[test]
    fn range_volume_indices_cover_the_window_and_cap_keeps_the_end() {
        let list = labels(&[
            "20:00:00", "20:05:00", "20:10:00", "20:15:00", "20:20:00", "20:25:00",
        ]);
        // Window 20:07–20:18: the volume scanning at the start (20:05)
        // through the last inside (20:15).
        assert_eq!(
            range_volume_indices(&list, "20:07:00", "20:18:00", 0, 200),
            Some((1, 3))
        );
        // Window edges off both ends of the day clamp to the listing.
        assert_eq!(
            range_volume_indices(&list, "00:00:00", "23:59:59", 0, 200),
            Some((0, 5))
        );
        // Cap trims the START (keep the track's end).
        assert_eq!(
            range_volume_indices(&list, "00:00:00", "23:59:59", 0, 3),
            Some((3, 5))
        );
        // Inverted/degenerate window collapses to one volume.
        assert_eq!(
            range_volume_indices(&list, "20:18:00", "20:07:00", 0, 200),
            Some((1, 1))
        );
        assert_eq!(
            range_volume_indices(&list, "20:07:00", "20:18:00", 0, 0),
            None
        );
        assert_eq!(
            range_volume_indices(&[], "20:07:00", "20:18:00", 0, 5),
            None
        );
    }

    #[test]
    fn range_volume_indices_pad_extends_each_side_and_clamps() {
        let list = labels(&[
            "20:00:00", "20:05:00", "20:10:00", "20:15:00", "20:20:00", "20:25:00",
        ]);
        // Bare window covers (1..=3); one scan of context each side.
        assert_eq!(
            range_volume_indices(&list, "20:07:00", "20:18:00", 1, 200),
            Some((0, 4))
        );
        // Pad clamps to the day's listing at both edges.
        assert_eq!(
            range_volume_indices(&list, "20:07:00", "20:18:00", 50, 200),
            Some((0, 5))
        );
        // Cap still trims the padded START (keep the track's end).
        assert_eq!(
            range_volume_indices(&list, "20:07:00", "20:18:00", 50, 3),
            Some((3, 5))
        );
    }

    #[test]
    fn window_labels_clamp_to_the_listed_date() {
        let listed = NaiveDate::from_ymd_opt(2026, 6, 11).unwrap();
        let start = Utc.with_ymd_and_hms(2026, 6, 11, 22, 50, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 6, 12, 0, 20, 0).unwrap();
        // End spills past midnight: clamp to the date's last second.
        assert_eq!(
            window_labels_for_date(listed, start, end),
            ("22:50:00".to_owned(), "23:59:59".to_owned())
        );
        // Start on the previous date clamps to 00:00:00.
        let listed_next = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        assert_eq!(
            window_labels_for_date(listed_next, start, end),
            ("00:00:00".to_owned(), "00:20:00".to_owned())
        );
    }

    #[test]
    fn volume_label_parses_archive_keys() {
        assert_eq!(
            volume_time_label("2026/06/09/KEAX/KEAX20260609_235423_V06"),
            "23:54:23"
        );
        assert_eq!(volume_time_label("garbage"), "??");
    }
}
