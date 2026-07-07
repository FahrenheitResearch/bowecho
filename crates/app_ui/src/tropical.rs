//! Tropical-cyclone map layer.
//!
//! Polls active storms worldwide (NHC + GDACS, via `data_source::tropical`) on
//! an interval into a [`TropicalState`], progressively fetches each storm's
//! track/cone geometry, renders the storm cards, and draws the map overlay
//! (position glyph + forecast track + cone of uncertainty).
//!
//! The background fetches use the app's [`WorkerSlot`] idiom (one job in
//! flight, drained every frame). The overlay draw is an `impl crate::ViewerApp`
//! method here — a sibling module can reach the crate-root paint helpers
//! (`crate::push_solid_open_line`, …) and `self.lon_lat_to_screen`, exactly
//! like `tor_tracks.rs` does.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use data_source::tropical::{self, Basin, Category, StormGeometry, TropicalCyclone};
use eframe::egui;
use ui_core::worker_slot::{SlotPoll, WorkerSlot};

use crate::sat_window::{self, SatNativeWindow};
use crate::sat_worker;

/// Re-poll cadence. NHC advisories update ~every 3–6 h (with intermediate
/// position updates) and GDACS a few times a day; 10 min keeps the card fresh
/// without hammering the sources.
pub const TROPICAL_REFRESH_SECONDS: u64 = 600;
/// Faster retry cadence used until the first successful fetch, or after a failed
/// one, so a slow/unavailable source (GDACS can lag) doesn't leave the panel
/// falsely showing "no active" for a full [`TROPICAL_REFRESH_SECONDS`] window.
pub const TROPICAL_RETRY_SECONDS: u64 = 20;
/// Age after which a cached storm geometry is refetched even without a newer
/// advisory time in the storms list — the backstop that keeps a long-running
/// session's forecast track honest (one extra fetch per storm per hour). NHC
/// reissues the TCM every 6 h; GDACS episodes bump their storm id, which
/// evicts+refetches on its own; JTWC warnings additionally refetch as soon as
/// the RSS feed advertises a higher warning number (every 6 h, 3 h near land).
pub const TROPICAL_GEOMETRY_MAX_AGE_SECONDS: u64 = 3600;

type StormsResult = std::result::Result<Vec<TropicalCyclone>, String>;
type GeometryResult = std::result::Result<GeometryFetch, String>;

/// One finished geometry fetch: which storm, the `advisory_time` and the
/// RSS-advertised JTWC warning number its storms-list record carried when the
/// fetch was SPAWNED (so a later, newer advisory/warning marks the entry
/// stale), and the parsed geometry.
struct GeometryFetch {
    id: String,
    advisory_time: Option<DateTime<Utc>>,
    warning_nr: Option<u32>,
    geometry: StormGeometry,
}

/// A cached [`StormGeometry`] plus the freshness bookkeeping that decides when
/// [`TropicalState::drive_geometry`] refetches it. NHC storm ids are stable
/// for the storm's whole life (`nhc:al142024`), so without this the first TCM
/// ever fetched would be pinned until app exit while NHC issues a new
/// forecast every 6 h — the current-position glyph kept moving but the
/// forecast dots/line/radii went days stale (audit #2). The JTWC warning
/// behind a West-Pacific storm has the same failure shape (a stable
/// `wpNNyyweb.txt` URL re-issued every 6 h), so the RSS-advertised warning
/// number is tracked the same way the advisory time is.
pub struct StormGeometryEntry {
    pub geometry: StormGeometry,
    /// When the fetch completed (drives the age-based refetch backstop).
    fetched_at: Instant,
    /// The storm's advisory time as of the fetch; `None` when the source
    /// didn't carry one.
    advisory_time: Option<DateTime<Utc>>,
    /// The RSS-advertised JTWC warning number as of the fetch; `None` for
    /// NHC/GDACS-only storms or while the RSS feed was unavailable.
    warning_nr: Option<u32>,
}

impl StormGeometryEntry {
    /// Whether this entry should be refetched: the storms list now carries a
    /// newer advisory time OR a higher JTWC warning number than the ones this
    /// geometry was fetched under, or the entry has outlived
    /// [`TROPICAL_GEOMETRY_MAX_AGE_SECONDS`].
    fn is_stale(
        &self,
        current_advisory: Option<DateTime<Utc>>,
        current_warning_nr: Option<u32>,
        now: Instant,
    ) -> bool {
        let newer_advisory = match (self.advisory_time, current_advisory) {
            (Some(cached), Some(current)) => current > cached,
            // The storms list learned an advisory time this fetch never saw.
            (None, Some(_)) => true,
            _ => false,
        };
        // JTWC re-issues the same warning URL under a bumped number; a number
        // the cached fetch never saw means the stored bulletin is superseded.
        // (None, Some) also covers a JTWC match appearing after the geometry
        // was first fetched (RSS outage, or the warning opened later).
        let newer_warning = match (self.warning_nr, current_warning_nr) {
            (Some(cached), Some(current)) => current > cached,
            (None, Some(_)) => true,
            _ => false,
        };
        newer_advisory
            || newer_warning
            || now.saturating_duration_since(self.fetched_at)
                >= Duration::from_secs(TROPICAL_GEOMETRY_MAX_AGE_SECONDS)
    }
}

/// The next storm whose geometry needs (re)fetching: it has a geometry URL and
/// either no cached entry yet or a stale one. Pure so the refetch policy is
/// unit-tested without a network.
fn storm_needing_geometry<'a>(
    storms: &'a [TropicalCyclone],
    geometry: &HashMap<String, StormGeometryEntry>,
    now: Instant,
) -> Option<&'a TropicalCyclone> {
    storms.iter().find(|storm| {
        storm.geometry_url.is_some()
            && geometry
                .get(&storm.id)
                .is_none_or(|entry| entry.is_stale(storm.advisory_time, storm.jtwc_warning_nr, now))
    })
}

/// All state for the tropical layer, owned by `ViewerApp.tropical`.
pub struct TropicalState {
    storms_rx: WorkerSlot<StormsResult>,
    geometry_rx: WorkerSlot<GeometryResult>,
    /// Active storms, strongest first (the merge sorts them).
    pub storms: Vec<TropicalCyclone>,
    /// Per-storm track/cone (+ fetch freshness), keyed by storm id, filled by
    /// the 2nd fetch and refetched when stale.
    pub geometry: HashMap<String, StormGeometryEntry>,
    /// Short human status for the panel header.
    pub status: String,
    /// Result of the most recent completed fetch: `None` until the first one
    /// finishes (so the panel can show "checking…" instead of "no active"),
    /// then `Some(true)` on success / `Some(false)` on failure (drives the
    /// faster retry cadence).
    last_fetch_ok: Option<bool>,
    last_refresh: Option<Instant>,
    /// A card asked the map to recenter here (lon, lat); ViewerApp drains it.
    pub focus_request: Option<(f32, f32)>,
    /// A card's 🛰 Vis/IR press waiting to be dispatched;
    /// [`crate::ViewerApp::drive_tropical_sat_view`] drains it.
    pub sat_view_request: Option<TcSatViewRequest>,
    /// The dispatched card request whose one-shot ingest is still running
    /// (disables the 🛰 buttons and spins the pressed card).
    pub sat_view_inflight: Option<TcSatInflight>,
    /// Monotonic ticket source correlating card requests with worker
    /// outcomes (stale outcomes from superseded requests are dropped).
    sat_view_ticket: u64,
}

impl Default for TropicalState {
    fn default() -> Self {
        Self {
            storms_rx: WorkerSlot::idle("tropical-cyclones"),
            geometry_rx: WorkerSlot::idle("tropical-geometry"),
            storms: Vec::new(),
            geometry: HashMap::new(),
            status: "Checking for active tropical cyclones…".to_owned(),
            last_fetch_ok: None,
            last_refresh: None,
            focus_request: None,
            sat_view_request: None,
            sat_view_inflight: None,
            sat_view_ticket: 0,
        }
    }
}

impl TropicalState {
    /// Kick a refresh if it's due and none is in flight; heartbeat so the
    /// interval keeps ticking on an otherwise-idle map.
    pub fn maybe_refresh(&mut self, ctx: &egui::Context) {
        // Until we have a good result, poll aggressively so a slow/failed first
        // fetch recovers in seconds instead of after the full 10-minute cycle.
        let interval = if self.last_fetch_ok == Some(true) {
            Duration::from_secs(TROPICAL_REFRESH_SECONDS)
        } else {
            Duration::from_secs(TROPICAL_RETRY_SECONDS)
        };
        let due = self
            .last_refresh
            .is_none_or(|last| last.elapsed() >= interval);
        if due && !self.storms_rx.in_flight() {
            self.last_refresh = Some(Instant::now());
            self.storms_rx.spawn(ctx, |tx| {
                let result = tropical_http_client()
                    .and_then(|client| tropical::fetch_active_cyclones(&client));
                let _ = tx.send(result);
            });
        }
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    /// Drain finished fetches. Must run every frame or a delivered result never
    /// clears the slot.
    pub fn poll(&mut self) {
        match self.storms_rx.poll() {
            SlotPoll::Ready(Ok(storms)) => {
                self.last_fetch_ok = Some(true);
                self.status = match storms.len() {
                    0 => "No active tropical cyclones".to_owned(),
                    1 => "1 active tropical cyclone".to_owned(),
                    n => format!("{n} active tropical cyclones"),
                };
                // Drop geometry for storms that are gone.
                self.geometry
                    .retain(|id, _| storms.iter().any(|s| &s.id == id));
                self.storms = storms;
            }
            SlotPoll::Ready(Err(err)) => {
                self.last_fetch_ok = Some(false);
                // Keep any storms we already have on screen; just note the retry.
                self.status = format!("Sources unavailable — retrying… ({err})");
            }
            SlotPoll::Idle | SlotPoll::Pending | SlotPoll::Disconnected => {}
        }
        if let SlotPoll::Ready(Ok(fetch)) = self.geometry_rx.poll() {
            self.geometry.insert(
                fetch.id,
                StormGeometryEntry {
                    geometry: fetch.geometry,
                    fetched_at: Instant::now(),
                    advisory_time: fetch.advisory_time,
                    warning_nr: fetch.warning_nr,
                },
            );
        }
        // Mirror each storm's parsed geometry payload (forecast points,
        // analysis wind radii, official warning identity + vitals) onto its
        // record. The geometry map is the transport (filled by the 2nd
        // fetch); a fresh storms list arrives with all of it empty, so
        // re-attach every poll — and for JTWC-covered storms the official
        // analysis intensity replaces GDACS's lagging severity (see
        // `sync_storm_with_geometry`).
        for storm in &mut self.storms {
            if let Some(entry) = self.geometry.get(&storm.id) {
                tropical::sync_storm_with_geometry(storm, &entry.geometry);
            }
        }
    }

    /// Progressively fetch missing or stale track/cone geometry, one storm at
    /// a time. Call each frame while the layer is visible. A failed refetch
    /// keeps the previous (stale) geometry on screen and simply tries again.
    pub fn drive_geometry(&mut self, ctx: &egui::Context) {
        if self.geometry_rx.in_flight() {
            return;
        }
        let next = storm_needing_geometry(&self.storms, &self.geometry, Instant::now());
        if let Some(storm) = next {
            let id = storm.id.clone();
            let source = storm.source;
            let advisory_time = storm.advisory_time;
            let warning_nr = storm.jtwc_warning_nr;
            let url = storm.geometry_url.clone().expect("checked is_some");
            // JTWC per-point forecast intensity for West-Pacific/Indian/Southern
            // storms (None for NHC basins, which carry it in their own TCM).
            let forecast_url = storm.forecast_url.clone();
            self.geometry_rx.spawn(ctx, move |tx| {
                let result = tropical_http_client()
                    .and_then(|client| {
                        tropical::fetch_storm_geometry(
                            &client,
                            source,
                            &url,
                            forecast_url.as_deref(),
                        )
                    })
                    .map(|geometry| GeometryFetch {
                        id,
                        advisory_time,
                        warning_nr,
                        geometry,
                    });
                let _ = tx.send(result);
            });
        }
    }

    /// The storm-cards panel body (rendered into a window/panel by ViewerApp).
    pub fn cards_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new(&self.status).weak());
        if self.storms.is_empty() {
            ui.add_space(4.0);
            match self.last_fetch_ok {
                // No completed fetch yet, or the last one failed — don't imply
                // an authoritative all-clear while we're still (re)trying.
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Checking NHC + GDACS for active storms…");
                    });
                }
                Some(false) => {
                    ui.label("Couldn't reach the storm sources — retrying…");
                }
                Some(true) => {
                    ui.label("Quiet across every basin right now.");
                }
            }
            return;
        }
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .max_height(520.0)
            .show(ui, |ui| {
                let storms = std::mem::take(&mut self.storms);
                for storm in &storms {
                    self.storm_card(ui, storm);
                    ui.add_space(6.0);
                }
                self.storms = storms;
            });
    }

    fn storm_card(&mut self, ui: &mut egui::Ui, storm: &TropicalCyclone) {
        let color = category_color(storm.category);
        egui::Frame::group(ui.style())
            .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.8)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&storm.name)
                            .strong()
                            .size(16.0)
                            .color(color),
                    );
                    if let Some(level) = &storm.alert_level {
                        ui.label(alert_badge(level));
                    }
                });
                ui.label(egui::RichText::new(&storm.classification).color(color));

                if let Some(wind) = storm.wind_summary() {
                    vital(ui, "Wind", &wind);
                }
                if let Some(pressure) = storm.pressure_summary() {
                    vital(ui, "Pressure", &pressure);
                }
                if let Some(motion) = storm.motion_summary() {
                    vital(ui, "Moving", &motion);
                }
                if let Some(areas) = &storm.affected_areas {
                    vital(ui, "Threatens", areas);
                }
                vital(
                    ui,
                    "Position",
                    &format!(
                        "{:.1}°{}, {:.1}°{}",
                        storm.position.lat.abs(),
                        if storm.position.lat >= 0.0 { "N" } else { "S" },
                        storm.position.lon.abs(),
                        if storm.position.lon >= 0.0 { "E" } else { "W" },
                    ),
                );

                // The official bulletin behind the numbers: which warning/
                // advisory, when it was issued (with age), and its analysis
                // time — so a stale intensity is never silent, e.g.
                // "JTWC Warning #25 · issued 07/0300Z (4 h ago) · position
                // 07/0000Z".
                if let Some(warning) = &storm.warning {
                    ui.label(
                        egui::RichText::new(warning.identity_summary(Utc::now()))
                            .small()
                            .weak(),
                    );
                }
                // The list source's own record (GDACS aggregate / NHC
                // CurrentStorms) — for GDACS+JTWC storms this dates the
                // track/cone/alert level, while the warning line above dates
                // the intensity.
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(storm.source.label()).small().weak());
                    if let Some(time) = storm.advisory_time {
                        ui.label(
                            egui::RichText::new(format!(
                                "· updated {}",
                                tropical::age_label(Utc::now(), time)
                            ))
                            .small()
                            .weak(),
                        );
                    }
                });

                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.small_button("📍 Focus").clicked() {
                        self.focus_request = Some((storm.position.lon, storm.position.lat));
                    }
                    // One-press satellite views of THIS storm: pick the
                    // covering geostationary satellite, ingest a
                    // native-resolution window centered on the storm, and
                    // show it (Satellite window + map layer) when it lands.
                    let busy = self.sat_view_inflight.is_some();
                    for (product, label, hover) in [
                        (
                            TcSatProduct::Vis,
                            "🛰 Vis",
                            "One press: pick the covering geostationary satellite \
                             (Himawari / GOES-East / GOES-West) and load a native-resolution \
                             TRUE-COLOR window centered on this storm. Opens the Satellite \
                             window and follows the frame onto the radar map. Daylight side only.",
                        ),
                        (
                            TcSatProduct::Ir,
                            "🛰 IR",
                            "One press: pick the covering geostationary satellite and load a \
                             Band-13 IR window centered on this storm, colored with the \
                             currently selected IR enhancement (BD, AVN, …). Works day and night.",
                        ),
                    ] {
                        let response = ui
                            .add_enabled(!busy, egui::Button::new(label).small())
                            .on_hover_text(hover)
                            .on_disabled_hover_text(
                                "a storm satellite load is already running — one at a time",
                            );
                        if response.clicked() {
                            self.sat_view_request = Some(TcSatViewRequest {
                                product,
                                storm_id: storm.id.clone(),
                                storm_name: storm.name.clone(),
                                basin: storm.basin,
                                lat: f64::from(storm.position.lat),
                                lon: f64::from(storm.position.lon),
                            });
                        }
                    }
                    // The pressed card wears the spinner while its ingest runs.
                    if let Some(inflight) = self
                        .sat_view_inflight
                        .as_ref()
                        .filter(|inflight| inflight.storm_id == storm.id)
                    {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new(format!("{}…", inflight.product.label()))
                                .small()
                                .weak(),
                        );
                    }
                    for (label, url) in external_links(storm) {
                        if ui.small_button(label).clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                        }
                    }
                });
            });
    }

    /// Next correlation ticket for a card satellite request.
    fn next_sat_view_ticket(&mut self) -> u64 {
        self.sat_view_ticket += 1;
        self.sat_view_ticket
    }
}

/// Geographic bounding box `[west, south, east, north]` (degrees) of a cone
/// ring, feeding `crate::cone_segment_jump_limit_px`.
fn cone_bbox_deg(cone: &[data_source::tropical::GeoPoint]) -> [f32; 4] {
    let mut bbox = [
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    ];
    for point in cone {
        bbox[0] = bbox[0].min(point.lon);
        bbox[1] = bbox[1].min(point.lat);
        bbox[2] = bbox[2].max(point.lon);
        bbox[3] = bbox[3].max(point.lat);
    }
    bbox
}

fn vital(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{label}: ")).weak());
        ui.label(value);
    });
}

/// External plot providers to open in the browser (credited, never scraped).
fn external_links(storm: &TropicalCyclone) -> Vec<(&'static str, String)> {
    let mut links = Vec::new();
    if let Some(report) = &storm.report_url {
        links.push(("📄 Advisory", report.clone()));
    }
    // Zoom Earth deep-links to a live-satellite view centered on the storm.
    links.push((
        "🛰 Zoom Earth",
        format!(
            "https://zoom.earth/#view={:.1},{:.1},6z",
            storm.position.lat, storm.position.lon
        ),
    ));
    links.push((
        "📈 Tropical Tidbits",
        "https://www.tropicaltidbits.com/storms/".to_owned(),
    ));
    links.push(("🌀 CyclonicWx", "https://www.cyclonicwx.com/".to_owned()));
    links
}

fn alert_badge(level: &str) -> egui::RichText {
    let color = match level.to_ascii_lowercase().as_str() {
        "red" => egui::Color32::from_rgb(232, 66, 66),
        "orange" => egui::Color32::from_rgb(240, 150, 40),
        "green" => egui::Color32::from_rgb(70, 190, 110),
        _ => egui::Color32::GRAY,
    };
    egui::RichText::new(format!(" {} ", level.to_uppercase()))
        .small()
        .strong()
        .background_color(color.gamma_multiply(0.35))
        .color(color)
}

/// Tropical intensity color (Saffir–Simpson-ish ramp), used by the cards and
/// the position glyph.
pub fn category_color(category: Option<Category>) -> egui::Color32 {
    match category {
        Some(Category::Five) => egui::Color32::from_rgb(255, 110, 245),
        Some(Category::Four) => egui::Color32::from_rgb(232, 66, 66),
        Some(Category::Three) => egui::Color32::from_rgb(245, 130, 50),
        Some(Category::Two) => egui::Color32::from_rgb(245, 200, 60),
        Some(Category::One) => egui::Color32::from_rgb(240, 240, 90),
        Some(Category::TropicalStorm) => egui::Color32::from_rgb(90, 210, 120),
        Some(Category::TropicalDepression) | None => egui::Color32::from_rgb(120, 190, 235),
    }
}

fn tropical_http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("BowEcho tropical layer (github.com/FahrenheitResearch/bowecho)")
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|err| err.to_string())
}

// ---------------------------------------------------------------------------
// One-press storm satellite view (the cards' 🛰 Vis / 🛰 IR buttons)
// ---------------------------------------------------------------------------

/// Native-window size for one-press storm satellite views, km per side:
/// wide enough for the eyewall plus the inner rainband field of a large
/// cyclone, small enough that the 0.5 km visible crop (~2000² px) stays
/// loop-friendly. Inside [`SatNativeWindow`]'s 50..2000 km domain.
pub const TC_SAT_WINDOW_KM: f64 = 1000.0;

/// Give up on a card spinner after this long without a worker outcome: a
/// first (uncached) full-disk visible scan is a few-hundred-MB download,
/// so minutes are normal; anything past this is a hung source and the
/// buttons unlock (the ingest itself still finishes or fails on its own).
const TC_SAT_INFLIGHT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Which one-press product a card asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcSatProduct {
    /// Native-window true color (daylight side only).
    Vis,
    /// Band-13 IR window through the current IR enhancement.
    Ir,
}

impl TcSatProduct {
    /// Short product label for card/status lines.
    pub fn label(self) -> &'static str {
        match self {
            Self::Vis => "True color",
            Self::Ir => "IR B13",
        }
    }
}

/// A card's 🛰 press waiting to be dispatched (drained once per frame by
/// [`crate::ViewerApp::drive_tropical_sat_view`]).
pub struct TcSatViewRequest {
    pub product: TcSatProduct,
    pub storm_id: String,
    pub storm_name: String,
    pub basin: Basin,
    pub lat: f64,
    pub lon: f64,
}

/// The dispatched card request whose one-shot ingest is still running.
pub struct TcSatInflight {
    pub ticket: u64,
    pub storm_id: String,
    pub storm_name: String,
    pub product: TcSatProduct,
    pub started: Instant,
}

/// The geostationary satellites the app can ingest, as storm coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcGeoSat {
    Himawari,
    GoesEast,
    GoesWest,
}

impl TcGeoSat {
    const ALL: [TcGeoSat; 3] = [Self::Himawari, Self::GoesEast, Self::GoesWest];

    /// Nominal sub-satellite longitude (the same constants the persisted
    /// native window's visibility gate uses).
    fn sub_lon_deg(self) -> f64 {
        match self {
            Self::Himawari => sat_window::AHI_NOMINAL_SUB_LON_DEG,
            Self::GoesEast => sat_window::GOES_EAST_SUB_LON_DEG,
            Self::GoesWest => sat_window::GOES_WEST_SUB_LON_DEG,
        }
    }

    /// Ingest slug for the operational slot (Himawari-9; GOES-19 East,
    /// GOES-18 West).
    fn satellite_slug(self) -> &'static str {
        match self {
            Self::Himawari => "h9",
            Self::GoesEast => "goes19",
            Self::GoesWest => "goes18",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Himawari => "Himawari-9",
            Self::GoesEast => "GOES-East",
            Self::GoesWest => "GOES-West",
        }
    }
}

/// Great-circle arc (degrees) from a geostationary sub-satellite point
/// (lat 0, `sub_lon_deg`) to a point — the ranking key when no basin rule
/// applies. Spherical, same formula as
/// [`sat_window::window_visible_from_sub_lon`]'s gate.
fn sub_lon_arc_deg(sub_lon_deg: f64, lat_deg: f64, lon_deg: f64) -> f64 {
    let lat = lat_deg.to_radians();
    let delta = (lon_deg - sub_lon_deg).to_radians();
    (lat.cos() * delta.cos())
        .clamp(-1.0, 1.0)
        .acos()
        .to_degrees()
}

/// Pick the covering geostationary satellite for a storm window: the
/// basin's operational satellite first (WPac → Himawari, EPac/CPac →
/// GOES-West, Atlantic → GOES-East — the agency assignments, which beat
/// raw longitude distance where the disks overlap), then every satellite
/// by increasing arc, taking the first whose disk can actually see the
/// window ([`sat_window::window_visible_from_sub_lon`], the same gate the
/// persisted native window uses). `None` when nothing in-app covers the
/// storm — Meteosat's longitudes, hidden until EUMETSAT access is
/// reliable.
fn covering_geo_satellite(basin: Basin, window: &SatNativeWindow) -> Option<TcGeoSat> {
    let clamped = window.clamped();
    let preferred = match basin {
        Basin::WestPacific => Some(TcGeoSat::Himawari),
        Basin::EastPacific | Basin::CentralPacific => Some(TcGeoSat::GoesWest),
        Basin::Atlantic => Some(TcGeoSat::GoesEast),
        Basin::NorthIndian | Basin::SouthIndian | Basin::SouthPacific | Basin::Other => None,
    };
    let mut by_arc = TcGeoSat::ALL;
    by_arc.sort_by(|a, b| {
        let arc = |sat: &TcGeoSat| {
            sub_lon_arc_deg(
                sat.sub_lon_deg(),
                clamped.center_lat_deg,
                clamped.center_lon_deg,
            )
        };
        arc(a).total_cmp(&arc(b))
    });
    preferred
        .into_iter()
        .chain(by_arc)
        .find(|sat| sat_window::window_visible_from_sub_lon(sat.sub_lon_deg(), &clamped))
}

/// What one card press sends: the chosen satellite plus the exact worker
/// request spec. Pure so satellite choice and request construction
/// unit-test without a `ViewerApp` or a network.
#[derive(Debug)]
pub(crate) enum TcSatPlan {
    /// v0.29.3 native-window AHI true color (the Bavi-proof machinery).
    HimawariVis(sat_worker::HimawariCompositeSpec),
    /// Native-window GOES natural-color composite.
    GoesVis(sat_worker::GoesCompositeSpec),
    /// Native-window AHI B13 Kelvin BT (live IR-enhancement recolor).
    HimawariIr(sat_worker::HimawariIrWindowSpec),
    /// Native-window GOES B13 baked through the current IR enhancement.
    GoesIr(sat_worker::GoesIrWindowSpec),
}

impl TcSatPlan {
    fn into_request(self) -> sat_worker::SatRequest {
        match self {
            Self::HimawariVis(spec) => sat_worker::SatRequest::IngestLatestHimawariComposite(spec),
            Self::GoesVis(spec) => sat_worker::SatRequest::IngestLatestGoesComposite(spec),
            Self::HimawariIr(spec) => sat_worker::SatRequest::IngestLatestHimawariIrWindow(spec),
            Self::GoesIr(spec) => sat_worker::SatRequest::IngestLatestGoesIrWindow(spec),
        }
    }
}

/// Build the one-press request for a storm: a [`TC_SAT_WINDOW_KM`] native
/// window centered on the storm's current position, on the covering
/// satellite, as the product the card asked for.
fn plan_tc_sat_view(
    product: TcSatProduct,
    basin: Basin,
    lat_deg: f64,
    lon_deg: f64,
    ticket: u64,
) -> Result<(TcGeoSat, TcSatPlan), String> {
    let window = SatNativeWindow {
        center_lat_deg: lat_deg,
        center_lon_deg: lon_deg,
        size_km: TC_SAT_WINDOW_KM,
    }
    .clamped();
    let Some(sat) = covering_geo_satellite(basin, &window) else {
        return Err(
            "no in-app geostationary satellite covers this storm (Meteosat/MTG is \
             temporarily unavailable)"
                .to_string(),
        );
    };
    let plan = match (sat, product) {
        (TcGeoSat::Himawari, TcSatProduct::Vis) => {
            TcSatPlan::HimawariVis(sat_worker::HimawariCompositeSpec {
                satellite: sat.satellite_slug().to_string(),
                style: "true_color".to_string(),
                window: Some(window),
                card_ticket: Some(ticket),
                ..Default::default()
            })
        }
        (TcGeoSat::Himawari, TcSatProduct::Ir) => {
            TcSatPlan::HimawariIr(sat_worker::HimawariIrWindowSpec {
                satellite: sat.satellite_slug().to_string(),
                band: 13,
                window,
                lookback_minutes: 180,
                as_of: None,
                card_ticket: Some(ticket),
            })
        }
        (TcGeoSat::GoesEast | TcGeoSat::GoesWest, TcSatProduct::Vis) => {
            TcSatPlan::GoesVis(sat_worker::GoesCompositeSpec {
                satellite: sat.satellite_slug().to_string(),
                // Full disk sees any storm the satellite covers (CONUS
                // misses most Atlantic/EPac tracks); the native window
                // keeps decode/compose/store window-sized regardless.
                sector: "fulldisk".to_string(),
                style: "natural_color".to_string(),
                window: Some(window),
                card_ticket: Some(ticket),
                ..Default::default()
            })
        }
        (TcGeoSat::GoesEast | TcGeoSat::GoesWest, TcSatProduct::Ir) => {
            TcSatPlan::GoesIr(sat_worker::GoesIrWindowSpec {
                satellite: sat.satellite_slug().to_string(),
                sector: "fulldisk".to_string(),
                band: 13,
                window,
                lookback_minutes: 180,
                as_of: None,
                card_ticket: Some(ticket),
            })
        }
    };
    Ok((sat, plan))
}

impl crate::ViewerApp {
    /// Draw the tropical overlay: cone + forecast track (from cached geometry)
    /// under the current-position intensity glyph. Both map paint sites call
    /// this (single-pane and per-cell).
    pub(crate) fn draw_tropical(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !self.app_settings.show_tropical || self.tropical.storms.is_empty() {
            return;
        }
        // A legitimate track/cone can span the viewport; cull only segments
        // longer than that (AEQD antimeridian teleports for W-Pacific storms).
        let jump_px = rect.width().max(rect.height());

        let mut shapes: Vec<egui::Shape> = Vec::new();
        for storm in &self.tropical.storms {
            let Some(geom) = self
                .tropical
                .geometry
                .get(&storm.id)
                .map(|entry| &entry.geometry)
            else {
                continue;
            };
            // Storms with official JTWC wind radii (West Pacific / Indian Ocean /
            // Southern Hemisphere) render the authentic JTWC product — the
            // 34-kt wind danger area + per-point wind rose — instead of the
            // generic GDACS cone (drawing both would stack two overlapping
            // envelopes). GDACS-only storms keep the cone.
            let has_radii = storm_has_wind_radii(storm);
            if has_radii {
                self.push_danger_area_shapes(&mut shapes, rect, storm);
            }
            if geom.cone.len() >= 3 && !has_radii {
                let ring: Vec<egui::Pos2> = geom
                    .cone
                    .iter()
                    .map(|p| self.lon_lat_to_screen(rect, p.lon, p.lat))
                    .collect();
                // The cone's jump limit is derived from its OWN geographic size
                // (not the viewport) so a wide, partly-off-screen cone still
                // draws instead of being culled all-or-nothing; only genuine
                // antimeridian teleports still trip the cull. See
                // `crate::cone_overlay_shapes`.
                shapes.extend(crate::cone_overlay_shapes(
                    &ring,
                    cone_bbox_deg(&geom.cone),
                    self.map_scale,
                    rect,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 26),
                    egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 150),
                    ),
                ));
            }
            // Draw each GDACS track segment on its own — they are short,
            // independently-oriented pieces; joining them into one polyline
            // zigzags and crosses the map with spurious connecting lines.
            for segment in &geom.track {
                if segment.len() < 2 {
                    continue;
                }
                let line: Vec<egui::Pos2> = segment
                    .iter()
                    .map(|p| self.lon_lat_to_screen(rect, p.lon, p.lat))
                    .collect();
                crate::push_solid_open_line(
                    &mut shapes,
                    &line,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(250, 250, 250)),
                    rect,
                    jump_px,
                );
            }
        }
        // Paint the cone of uncertainty + observed track (built above) BEFORE
        // the forecast overlay so the dots/line sit on top. (Regression guard:
        // this `painter.extend(shapes)` was dropped when the forecast overlay
        // was added, which silently hid the cone and past track.)
        painter.extend(shapes);

        // Forecast track: a thin line joining the current position to each
        // official forecast point, drawn under the dots.
        let mut forecast_lines: Vec<egui::Shape> = Vec::new();
        for storm in &self.tropical.storms {
            if storm.forecast.is_empty() {
                continue;
            }
            let mut path: Vec<egui::Pos2> = Vec::with_capacity(storm.forecast.len() + 1);
            path.push(self.lon_lat_to_screen(rect, storm.position.lon, storm.position.lat));
            for point in &storm.forecast {
                path.push(self.lon_lat_to_screen(rect, point.position.lon, point.position.lat));
            }
            crate::push_solid_open_line(
                &mut forecast_lines,
                &path,
                egui::Stroke::new(
                    1.5,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 170),
                ),
                rect,
                jump_px,
            );
        }
        painter.extend(forecast_lines);

        // JTWC wind-radii "rose": at the current position and each forecast
        // point, the 34-kt (outer/faint), 50-kt and 64-kt (inner/bright)
        // per-quadrant arcs, drawn under the dots. Only storms carrying radii
        // (West-Pacific etc.) contribute; the danger area already drew below.
        let mut radii_shapes: Vec<egui::Shape> = Vec::new();
        for storm in &self.tropical.storms {
            self.push_wind_radii_shapes(&mut radii_shapes, rect, storm);
        }
        painter.extend(radii_shapes);

        // Forecast dots, colored by each point's Saffir–Simpson category. NHC
        // (TCM) and JTWC-matched West-Pacific/Indian/Southern storms carry real
        // per-point max wind; any point still lacking it inherits the storm's
        // current category. Track the dot nearest the cursor for a stats tooltip.
        let hover = painter
            .ctx()
            .pointer_hover_pos()
            .filter(|p| rect.contains(*p));
        let mut nearest: Option<(f32, egui::Pos2, Vec<String>, egui::Color32)> = None;
        for storm in &self.tropical.storms {
            for point in &storm.forecast {
                let pos = self.lon_lat_to_screen(rect, point.position.lon, point.position.lat);
                if !rect.expand(40.0).contains(pos) {
                    continue;
                }
                let category = point
                    .max_wind_kt
                    .map(Category::from_wind_kt)
                    .or(storm.category);
                let color = category_color(category);
                painter.circle_filled(pos, 5.0, color);
                painter.circle_stroke(pos, 5.0, egui::Stroke::new(1.5, egui::Color32::BLACK));
                if let Some(hp) = hover {
                    let dist = hp.distance(pos);
                    if dist <= 9.0 && nearest.as_ref().is_none_or(|(best, ..)| dist < *best) {
                        nearest = Some((
                            dist,
                            pos,
                            forecast_tooltip_lines(storm, point, category),
                            color,
                        ));
                    }
                }
            }
        }

        // Current-position glyph (drawn last so it sits above the forecast).
        for storm in &self.tropical.storms {
            let pos = self.lon_lat_to_screen(rect, storm.position.lon, storm.position.lat);
            if !rect.expand(60.0).contains(pos) {
                continue;
            }
            let color = category_color(storm.category);
            painter.circle_filled(pos, 6.0, color);
            painter.circle_stroke(pos, 6.0, egui::Stroke::new(1.5, egui::Color32::BLACK));
            crate::draw_halo_text(
                painter,
                pos + egui::vec2(10.0, 0.0),
                egui::Align2::LEFT_CENTER,
                &storm.name,
                egui::FontId::proportional(12.0),
                color,
                egui::Color32::BLACK,
            );
        }

        if let Some((_, anchor, lines, accent)) = nearest {
            draw_forecast_tooltip(painter, anchor, &lines, accent);
        }
    }

    /// The 34-kt wind danger area (USN ship-avoidance swath): a translucent
    /// teal fill + red outline enclosing every 34-kt gale field along the
    /// track — the tapered envelope of the per-point gale circles laid along
    /// the forecast polyline, so it follows a recurving track instead of
    /// straight-lining first circle to last (the v0.29.2 "fat cone on Bavi's
    /// recurve" bug; see `data_source::tropical::track_circle_envelope`).
    /// Built in geographic space (great-circle offsets) then projected,
    /// reusing `crate::cone_overlay_shapes` so the same wide-shape jump-cull
    /// allowance keeps a basin-spanning envelope from being culled; the fill
    /// path ear-clips, so the now-concave inner bend renders correctly.
    fn push_danger_area_shapes(
        &self,
        shapes: &mut Vec<egui::Shape>,
        rect: egui::Rect,
        storm: &TropicalCyclone,
    ) {
        let points = std::iter::once((storm.position, storm.current_wind_radii.as_slice())).chain(
            storm
                .forecast
                .iter()
                .map(|p| (p.position, p.wind_radii.as_slice())),
        );
        let envelope = tropical::danger_area_34kt(points);
        if envelope.len() < 3 {
            return;
        }
        let ring: Vec<egui::Pos2> = envelope
            .iter()
            .map(|p| self.lon_lat_to_screen(rect, p.lon, p.lat))
            .collect();
        shapes.extend(crate::cone_overlay_shapes(
            &ring,
            cone_bbox_deg(&envelope),
            self.map_scale,
            rect,
            egui::Color32::from_rgba_unmultiplied(30, 200, 190, 28),
            egui::Stroke::new(1.5, egui::Color32::from_rgba_unmultiplied(235, 70, 70, 190)),
        ));
    }

    /// Per-quadrant 34/50/64-kt wind-radii arcs at the current position and each
    /// forecast point. Each threshold is a closed "wind rose" outline built from
    /// its NE/SE/SW/NW radii via great-circle offsets, then projected. The
    /// per-ring jump limit is derived from the ring's own geographic bbox so it
    /// survives at any zoom (as the cone does).
    fn push_wind_radii_shapes(
        &self,
        shapes: &mut Vec<egui::Shape>,
        rect: egui::Rect,
        storm: &TropicalCyclone,
    ) {
        let sets = std::iter::once((storm.position, &storm.current_wind_radii))
            .chain(storm.forecast.iter().map(|p| (p.position, &p.wind_radii)));
        for (center, radii_set) in sets {
            for radii in radii_set {
                let geo_ring = tropical::wind_radii_ring(center, radii, 8);
                if geo_ring.len() < 3 {
                    continue;
                }
                let ring: Vec<egui::Pos2> = geo_ring
                    .iter()
                    .map(|p| self.lon_lat_to_screen(rect, p.lon, p.lat))
                    .collect();
                let jump = crate::cone_segment_jump_limit_px(
                    cone_bbox_deg(&geo_ring),
                    self.map_scale,
                    rect,
                );
                crate::push_solid_closed_line(
                    shapes,
                    &ring,
                    egui::Stroke::new(1.2, wind_radii_color(radii.kt)),
                    rect,
                    jump,
                );
            }
        }
    }

    /// Dispatch the storm cards' one-press satellite requests and retire
    /// their outcomes. One call per frame from the update loop.
    ///
    /// A press ends with imagery on screen with no further clicks: the
    /// Satellite window opens/raises (its per-frame pass owns the response
    /// pump that installs finished frames), the map recenters on the storm,
    /// map-follow turns on, and the planned one-shot ingest auto-selects
    /// its frame in the player + map when it lands
    /// (`SatResponse::SelectFrame`). While it runs, the pressed card wears
    /// a spinner and every card's 🛰 buttons disable; the worker reports
    /// the outcome on the card-only channel
    /// ([`sat_worker::SatWorker::try_recv_card_outcome`]), with a timeout
    /// backstop so a hung source can't disable the buttons forever.
    pub(crate) fn drive_tropical_sat_view(&mut self, ctx: &egui::Context) {
        // Retire the in-flight request first so its outcome re-enables the
        // card buttons on this very frame.
        if let Some(inflight) = &self.tropical.sat_view_inflight {
            let mut done = false;
            if let Some(sat) = &self.sat {
                while let Some(outcome) = sat.try_recv_card_outcome() {
                    // Stale tickets (outcomes of superseded/timed-out
                    // requests) are dropped; only the current one retires.
                    if outcome.ticket != inflight.ticket {
                        continue;
                    }
                    // Success already surfaced through the ingest's own
                    // summary note; failures get an explicit status line
                    // (the pump's notes only run while the Satellite
                    // window is open).
                    if let Err(message) = &outcome.result {
                        self.status = format!(
                            "Satellite: {} {} failed — {message}",
                            inflight.storm_name,
                            inflight.product.label()
                        );
                    }
                    done = true;
                }
            }
            if !done && inflight.started.elapsed() >= TC_SAT_INFLIGHT_TIMEOUT {
                done = true;
            }
            if done {
                self.tropical.sat_view_inflight = None;
            }
        }

        let Some(request) = self.tropical.sat_view_request.take() else {
            return;
        };
        if self.tropical.sat_view_inflight.is_some() {
            // Buttons are disabled while in flight, so this only guards a
            // request raced against a not-yet-retired one: never queue.
            return;
        }
        let ticket = self.tropical.next_sat_view_ticket();
        let (choice, plan) = match plan_tc_sat_view(
            request.product,
            request.basin,
            request.lat,
            request.lon,
            ticket,
        ) {
            Ok(planned) => planned,
            Err(message) => {
                self.status = format!("Satellite: {} — {message}", request.storm_name);
                return;
            }
        };

        // Open/raise the Satellite window (nothing can land while it is
        // closed: its per-frame pass runs the response pump), recenter the
        // map on the storm, and follow the player onto the map layer.
        self.show_satellite = true;
        self.ensure_satellite_worker(ctx);
        self.sat_map_follow = true;
        self.tropical.focus_request = Some((request.lon as f32, request.lat as f32));

        let title = match request.product {
            TcSatProduct::Vis => format!("{} — True color", request.storm_name),
            TcSatProduct::Ir => format!(
                "{} — IR B13 · {}",
                request.storm_name,
                self.sat_ir_enhancement.label()
            ),
        };
        let Some(sat) = &self.sat else {
            self.status = format!("Satellite: {title} — worker unavailable");
            return;
        };
        sat.send(plan.into_request());
        self.status = format!("Satellite: {title} · loading via {}", choice.label());
        self.sat_panel
            .apply_note(format!("{title}: queued via {}", choice.label()));
        self.tropical.sat_view_inflight = Some(TcSatInflight {
            ticket,
            storm_id: request.storm_id,
            storm_name: request.storm_name,
            product: request.product,
            started: Instant::now(),
        });
    }
}

/// Whether a storm carries official quadrant wind radii (a matched JTWC
/// warning), which switches its overlay from the generic GDACS cone to the
/// authentic wind-rose + 34-kt danger-area rendering.
fn storm_has_wind_radii(storm: &TropicalCyclone) -> bool {
    !storm.current_wind_radii.is_empty() || storm.forecast.iter().any(|p| !p.wind_radii.is_empty())
}

/// Stroke color for a JTWC wind-radii threshold — the magenta family the JTWC
/// warning graphic uses: brightest hot-pink for the tight 64-kt hurricane-force
/// core, softer magenta for 50-kt storm force, and a faint violet for the wide
/// 34-kt gale ring (kept faint so the danger area and dots stay readable).
fn wind_radii_color(kt: u16) -> egui::Color32 {
    match kt {
        64 => egui::Color32::from_rgba_unmultiplied(255, 60, 130, 235),
        50 => egui::Color32::from_rgba_unmultiplied(255, 110, 205, 205),
        _ => egui::Color32::from_rgba_unmultiplied(210, 150, 235, 150),
    }
}

/// The hover-stats lines for one forecast point: valid time (UTC + viewer
/// local), max wind (kt + mph) where the office gives it, and the category.
fn forecast_tooltip_lines(
    storm: &TropicalCyclone,
    point: &tropical::ForecastPoint,
    category: Option<Category>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(valid) = point.valid_time {
        let local = valid.with_timezone(&chrono::Local);
        lines.push(format!(
            "{}  ({} local)",
            valid.format("%a %d %b %H:%MZ"),
            local.format("%H:%M")
        ));
    }
    match point.max_wind_kt {
        Some(kt) => lines.push(format!(
            "{:.0} kt · {:.0} mph",
            kt,
            kt / tropical::KT_PER_MPH
        )),
        None => lines.push("intensity: storm's current estimate".to_owned()),
    }
    if let Some(category) = category {
        lines.push(category.label(storm.basin));
    }
    // Which official bulletin this dot came from ("JTWC Warning #25").
    if let Some(warning) = &storm.warning {
        lines.push(warning.product_label());
    }
    lines
}

/// A small dark stats card anchored at a forecast dot.
fn draw_forecast_tooltip(
    painter: &egui::Painter,
    anchor: egui::Pos2,
    lines: &[String],
    accent: egui::Color32,
) {
    let font = egui::FontId::proportional(12.0);
    let galleys: Vec<_> = lines
        .iter()
        .map(|line| painter.layout_no_wrap(line.clone(), font.clone(), egui::Color32::WHITE))
        .collect();
    let pad = 6.0;
    let width = galleys.iter().map(|g| g.size().x).fold(0.0_f32, f32::max) + pad * 2.0;
    let height = galleys.iter().map(|g| g.size().y).sum::<f32>() + pad * 2.0;
    let origin = anchor + egui::vec2(12.0, -height - 8.0);
    let panel = egui::Rect::from_min_size(origin, egui::vec2(width, height));
    painter.rect_filled(
        panel,
        4.0,
        egui::Color32::from_rgba_unmultiplied(12, 15, 20, 235),
    );
    painter.rect_stroke(
        panel,
        4.0,
        egui::Stroke::new(1.0, accent),
        egui::StrokeKind::Outside,
    );
    let mut y = origin.y + pad;
    for galley in galleys {
        let advance = galley.size().y;
        painter.galley(egui::pos2(origin.x + pad, y), galley, egui::Color32::WHITE);
        y += advance;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use data_source::tropical::{Basin, GeoPoint, Source, WindRadii};

    fn advisory(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 10, 8, hour, 0, 0).unwrap()
    }

    fn test_storm(id: &str, advisory_time: Option<DateTime<Utc>>) -> TropicalCyclone {
        TropicalCyclone {
            id: id.to_owned(),
            name: "Milton".to_owned(),
            basin: Basin::Atlantic,
            source: Source::Nhc,
            classification: "Hurricane".to_owned(),
            category: None,
            position: GeoPoint {
                lon: -87.5,
                lat: 22.7,
            },
            max_wind_kt: Some(145.0),
            gust_kt: None,
            min_pressure_mb: None,
            movement_dir_deg: None,
            movement_speed_kt: None,
            advisory_time,
            alert_level: None,
            affected_areas: None,
            forecast: Vec::new(),
            current_wind_radii: Vec::new(),
            cone: Vec::new(),
            report_url: None,
            geometry_url: Some("https://www.nhc.noaa.gov/text/MIATCMAT4.shtml".to_owned()),
            forecast_url: None,
            warning: None,
            jtwc_warning_nr: None,
        }
    }

    fn entry(
        advisory_time: Option<DateTime<Utc>>,
        warning_nr: Option<u32>,
        fetched_at: Instant,
    ) -> StormGeometryEntry {
        StormGeometryEntry {
            geometry: StormGeometry::default(),
            fetched_at,
            advisory_time,
            warning_nr,
        }
    }

    /// Audit #2 regression: the stable NHC id (`nhc:al142024`) used to pin the
    /// first TCM fetched for the storm's whole life; a fresh storms list with
    /// a NEWER advisory time must mark the cached geometry stale.
    #[test]
    fn geometry_refetches_when_storm_advisory_is_newer() {
        let now = Instant::now();
        let mut geometry = HashMap::new();
        geometry.insert(
            "nhc:al142024".to_owned(),
            entry(Some(advisory(15)), None, now),
        );
        let storms = vec![test_storm("nhc:al142024", Some(advisory(21)))];
        let picked = storm_needing_geometry(&storms, &geometry, now)
            .expect("newer advisory forces a refetch");
        assert_eq!(picked.id, "nhc:al142024");
    }

    #[test]
    fn geometry_fresh_entry_is_not_refetched() {
        let now = Instant::now();
        let mut geometry = HashMap::new();
        geometry.insert(
            "nhc:al142024".to_owned(),
            entry(Some(advisory(21)), None, now),
        );
        // Same advisory as cached, entry just fetched: nothing to do.
        let storms = vec![test_storm("nhc:al142024", Some(advisory(21)))];
        assert!(storm_needing_geometry(&storms, &geometry, now).is_none());
        // An OLDER advisory in the list (feed hiccup) must not refetch either.
        let storms = vec![test_storm("nhc:al142024", Some(advisory(15)))];
        assert!(storm_needing_geometry(&storms, &geometry, now).is_none());
    }

    /// The age backstop refetches even when the feed's advisory time never
    /// moves (or is absent on both sides).
    #[test]
    fn geometry_refetches_after_max_age_backstop() {
        let fetched_at = Instant::now();
        let mut geometry = HashMap::new();
        geometry.insert("nhc:al142024".to_owned(), entry(None, None, fetched_at));
        let storms = vec![test_storm("nhc:al142024", None)];

        let just_before = fetched_at + Duration::from_secs(TROPICAL_GEOMETRY_MAX_AGE_SECONDS - 1);
        assert!(storm_needing_geometry(&storms, &geometry, just_before).is_none());

        let at_age = fetched_at + Duration::from_secs(TROPICAL_GEOMETRY_MAX_AGE_SECONDS);
        assert!(storm_needing_geometry(&storms, &geometry, at_age).is_some());
    }

    #[test]
    fn geometry_missing_entry_is_fetched_and_urlless_storms_are_skipped() {
        let now = Instant::now();
        let geometry = HashMap::new();
        let mut no_url = test_storm("gdacs:1:1", None);
        no_url.geometry_url = None;
        let storms = vec![no_url, test_storm("nhc:al142024", Some(advisory(21)))];
        let picked = storm_needing_geometry(&storms, &geometry, now).expect("uncached storm");
        assert_eq!(picked.id, "nhc:al142024");
    }

    /// A cached entry fetched before the source published any advisory time
    /// goes stale as soon as the storms list carries one.
    #[test]
    fn geometry_refetches_when_advisory_first_appears() {
        let now = Instant::now();
        let mut geometry = HashMap::new();
        geometry.insert("nhc:al142024".to_owned(), entry(None, None, now));
        let storms = vec![test_storm("nhc:al142024", Some(advisory(21)))];
        assert!(storm_needing_geometry(&storms, &geometry, now).is_some());
    }

    /// The JTWC face of audit #2 (the "Bavi stuck at the old warning" bug):
    /// the warning URL (`wp0926web.txt`) is stable while JTWC re-issues under
    /// a bumped warning number every 6 h, and GDACS's advisory time can sit
    /// still the whole while — the RSS-advertised number must trigger the
    /// refetch on its own.
    #[test]
    fn geometry_refetches_when_jtwc_warning_number_advances() {
        let now = Instant::now();
        let mut geometry = HashMap::new();
        // Fetched under Warning #21, same (unmoving) GDACS advisory time.
        geometry.insert(
            "gdacs:1001279:17".to_owned(),
            entry(Some(advisory(15)), Some(21), now),
        );
        let mut storm = test_storm("gdacs:1001279:17", Some(advisory(15)));
        storm.source = Source::Gdacs;
        storm.jtwc_warning_nr = Some(25);
        let storms = vec![storm];
        let picked = storm_needing_geometry(&storms, &geometry, now)
            .expect("a higher advertised warning number forces a refetch");
        assert_eq!(picked.id, "gdacs:1001279:17");

        // Same number → fresh; an RSS hiccup to an OLDER number must not
        // refetch either (mirrors the older-advisory guard).
        for stale_nr in [Some(21), Some(20)] {
            let mut storm = test_storm("gdacs:1001279:17", Some(advisory(15)));
            storm.source = Source::Gdacs;
            storm.jtwc_warning_nr = stale_nr;
            let storms = vec![storm];
            assert!(
                storm_needing_geometry(&storms, &geometry, now).is_none(),
                "warning nr {stale_nr:?} must not refetch"
            );
        }
    }

    /// A geometry cached before the storm had a JTWC match (RSS outage, or
    /// JTWC opened the warning later) refetches as soon as the RSS advertises
    /// one, so the enrichment isn't stuck waiting for the 1-h backstop.
    #[test]
    fn geometry_refetches_when_jtwc_match_first_appears() {
        let now = Instant::now();
        let mut geometry = HashMap::new();
        geometry.insert(
            "gdacs:1001279:17".to_owned(),
            entry(Some(advisory(15)), None, now),
        );
        let mut storm = test_storm("gdacs:1001279:17", Some(advisory(15)));
        storm.source = Source::Gdacs;
        storm.jtwc_warning_nr = Some(25);
        let storms = vec![storm];
        assert!(storm_needing_geometry(&storms, &geometry, now).is_some());
    }

    /// Audit #3 companion: a storm whose forecast points carry NHC TCM radii
    /// flips the renderer to the wind-rose + danger-area path.
    #[test]
    fn nhc_radii_flip_the_wind_rose_render_path() {
        let mut storm = test_storm("nhc:al142024", None);
        assert!(!storm_has_wind_radii(&storm), "bare storm: no radii");
        storm.current_wind_radii = vec![WindRadii {
            kt: 34,
            ne_nm: 70.0,
            se_nm: 80.0,
            sw_nm: 80.0,
            nw_nm: 120.0,
        }];
        assert!(storm_has_wind_radii(&storm));
    }

    fn tc_window(lat: f64, lon: f64) -> SatNativeWindow {
        SatNativeWindow {
            center_lat_deg: lat,
            center_lon_deg: lon,
            size_km: TC_SAT_WINDOW_KM,
        }
    }

    /// One-press satellite selection: basin rules first (WPac → Himawari,
    /// EPac/CPac → GOES-West, Atlantic → GOES-East), nearest-visible disk
    /// for basins without a rule, and an honest `None` where no in-app
    /// satellite can see the storm (Meteosat's slot is not in the app).
    #[test]
    fn tc_sat_selection_covers_basins_and_dateline() {
        // Bavi-class WPac storm.
        assert_eq!(
            covering_geo_satellite(Basin::WestPacific, &tc_window(25.0, 130.0)),
            Some(TcGeoSat::Himawari)
        );
        // Atlantic (Gulf) storm.
        assert_eq!(
            covering_geo_satellite(Basin::Atlantic, &tc_window(22.7, -87.5)),
            Some(TcGeoSat::GoesEast)
        );
        // EPac at 105 W: GOES-East is NEARER in longitude (29.8° vs 32.0°),
        // but the basin's operational satellite is GOES-West and must win.
        assert_eq!(
            covering_geo_satellite(Basin::EastPacific, &tc_window(15.0, -105.0)),
            Some(TcGeoSat::GoesWest)
        );
        assert_eq!(
            covering_geo_satellite(Basin::CentralPacific, &tc_window(20.0, -155.0)),
            Some(TcGeoSat::GoesWest)
        );
        // Dateline, no basin rule: nearest visible disk on either side.
        assert_eq!(
            covering_geo_satellite(Basin::Other, &tc_window(10.0, 179.0)),
            Some(TcGeoSat::Himawari)
        );
        assert_eq!(
            covering_geo_satellite(Basin::Other, &tc_window(10.0, -175.0)),
            Some(TcGeoSat::GoesWest)
        );
        // South Pacific storm just east of the dateline.
        assert_eq!(
            covering_geo_satellite(Basin::SouthPacific, &tc_window(-15.0, -170.0)),
            Some(TcGeoSat::GoesWest)
        );
        // Bay of Bengal: only Himawari's disk reaches it (arc ~52°).
        assert_eq!(
            covering_geo_satellite(Basin::NorthIndian, &tc_window(15.0, 90.0)),
            Some(TcGeoSat::Himawari)
        );
        // Arabian Sea / Meteosat territory: nothing in-app covers it.
        assert_eq!(
            covering_geo_satellite(Basin::NorthIndian, &tc_window(15.0, 55.0)),
            None
        );
    }

    /// The card window is a legal native window everywhere: the size sits
    /// inside the SatNativeWindow domain and dateline-adjacent centers stay
    /// normalized (clamped() is applied inside the plan).
    #[test]
    fn tc_sat_window_is_in_domain_and_dateline_safe() {
        assert!(
            (SatNativeWindow::MIN_SIZE_KM..=SatNativeWindow::MAX_SIZE_KM)
                .contains(&TC_SAT_WINDOW_KM)
        );
        let (sat, plan) = plan_tc_sat_view(TcSatProduct::Ir, Basin::WestPacific, 18.0, -178.5, 3)
            .expect("dateline WPac storm is covered");
        assert_eq!(sat, TcGeoSat::Himawari, "GDACS WPac extends past 180");
        match plan {
            TcSatPlan::HimawariIr(spec) => {
                assert!((spec.window.center_lon_deg - (-178.5)).abs() < 1e-9);
                assert!((spec.window.center_lat_deg - 18.0).abs() < 1e-9);
            }
            _ => panic!("WPac IR must plan a Himawari IR window"),
        }
    }

    /// Request construction: every basin/product pair sends the right spec
    /// with a storm-centered native window and the correlation ticket.
    #[test]
    fn tc_sat_plan_builds_windowed_requests() {
        // WPac Vis: the v0.29.3 native-window true-color machinery on h9.
        let (sat, plan) = plan_tc_sat_view(TcSatProduct::Vis, Basin::WestPacific, 25.3, 131.2, 7)
            .expect("covered");
        assert_eq!(sat, TcGeoSat::Himawari);
        match plan {
            TcSatPlan::HimawariVis(spec) => {
                assert_eq!(spec.satellite, "h9");
                assert_eq!(spec.style, "true_color");
                assert_eq!(spec.card_ticket, Some(7));
                let window = spec.window.expect("native window attached");
                assert!((window.center_lat_deg - 25.3).abs() < 1e-9);
                assert!((window.center_lon_deg - 131.2).abs() < 1e-9);
                assert!((window.size_km - TC_SAT_WINDOW_KM).abs() < 1e-9);
            }
            _ => panic!("WPac Vis must plan a Himawari composite"),
        }
        // WPac IR: windowed single-band B13 (live-recolorable Kelvin BT).
        let (_, plan) = plan_tc_sat_view(TcSatProduct::Ir, Basin::WestPacific, 25.3, 131.2, 8)
            .expect("covered");
        match plan {
            TcSatPlan::HimawariIr(spec) => {
                assert_eq!(spec.satellite, "h9");
                assert_eq!(spec.band, 13);
                assert_eq!(spec.card_ticket, Some(8));
                assert!((spec.window.center_lon_deg - 131.2).abs() < 1e-9);
            }
            _ => panic!("WPac IR must plan a Himawari IR window"),
        }
        // Atlantic Vis: GOES-East, full disk (CONUS misses most tracks),
        // natural color, windowed.
        let (sat, plan) =
            plan_tc_sat_view(TcSatProduct::Vis, Basin::Atlantic, 22.7, -87.5, 9).expect("covered");
        assert_eq!(sat, TcGeoSat::GoesEast);
        match plan {
            TcSatPlan::GoesVis(spec) => {
                assert_eq!(spec.satellite, "goes19");
                assert_eq!(spec.sector, "fulldisk");
                assert_eq!(spec.style, "natural_color");
                assert_eq!(spec.card_ticket, Some(9));
                let window = spec.window.expect("native window attached");
                assert!((window.center_lat_deg - 22.7).abs() < 1e-9);
                assert!((window.center_lon_deg - (-87.5)).abs() < 1e-9);
            }
            _ => panic!("Atlantic Vis must plan a GOES composite"),
        }
        // EPac IR: GOES-West enhanced-IR window.
        let (sat, plan) = plan_tc_sat_view(TcSatProduct::Ir, Basin::EastPacific, 15.0, -105.0, 10)
            .expect("covered");
        assert_eq!(sat, TcGeoSat::GoesWest);
        match plan {
            TcSatPlan::GoesIr(spec) => {
                assert_eq!(spec.satellite, "goes18");
                assert_eq!(spec.sector, "fulldisk");
                assert_eq!(spec.band, 13);
                assert_eq!(spec.card_ticket, Some(10));
                assert!((spec.window.center_lon_deg - (-105.0)).abs() < 1e-9);
            }
            _ => panic!("EPac IR must plan a GOES IR window"),
        }
        // Uncovered storm: an explicit, honest error (never a silent no-op).
        let err = plan_tc_sat_view(TcSatProduct::Ir, Basin::NorthIndian, 15.0, 55.0, 11)
            .expect_err("Arabian Sea west is out of every in-app disk");
        assert!(err.contains("Meteosat"), "{err}");
    }

    /// Tickets are monotonic and start above the initial inflight-free
    /// state, so a stale outcome can never match a fresh request.
    #[test]
    fn tc_sat_tickets_are_monotonic() {
        let mut state = TropicalState::default();
        assert!(state.sat_view_request.is_none());
        assert!(state.sat_view_inflight.is_none());
        let first = state.next_sat_view_ticket();
        let second = state.next_sat_view_ticket();
        assert!(second > first && first > 0);
    }
}
