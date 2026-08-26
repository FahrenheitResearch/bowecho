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

const SAT_STALE_DRAPE_GRID: usize = 8;
const SAT_RENDER_RETRY_BASE: Duration = Duration::from_secs(1);
const SAT_RENDER_RETRY_MAX_SECS: u64 = 16;

// GOES-R nominal fixed-grid geometry (the same values carried by operational
// ABI L1b/L2 NetCDF files). `perspective_point_height` is measured above the
// ellipsoid; the horizon calculation needs distance from Earth's center.
const GOES_NOMINAL_PERSPECTIVE_HEIGHT_M: f64 = 35_786_023.0;
const GOES_NOMINAL_SEMI_MAJOR_M: f64 = 6_378_137.0;
const GOES_NOMINAL_SEMI_MINOR_M: f64 = 6_356_752.314_14;
const GOES_FULL_DISK_FIT_MARGIN_POINTS: f32 = 16.0;
/// Only the explicit Full Disk action may cross the ordinary 7 px/degree map
/// floor. Keeping a separate conservative floor avoids making the global map
/// zoom capable of opening an almost-antipodal AEQD view.
const GOES_FULL_DISK_FIT_MIN_SCALE: f32 = 1.5;

#[derive(Clone, Copy, Debug, PartialEq)]
struct GoesFullDiskGeometry {
    center_lat_deg: f64,
    center_lon_deg: f64,
    east_limb_lon_deg: f64,
    west_limb_lon_deg: f64,
    north_limb_lat_deg: f64,
    south_limb_lat_deg: f64,
    max_limb_arc_deg: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct GoesFullDiskMapFit {
    center_lat_deg: f32,
    center_lon_deg: f32,
    map_scale: f32,
    limb_radius_points: f32,
}

fn normalize_longitude_f64(longitude: f64) -> f64 {
    (longitude + 180.0).rem_euclid(360.0) - 180.0
}

fn longitude_delta_degrees(left: f64, right: f64) -> f64 {
    normalize_longitude_f64(left - right).abs()
}

/// Nominal surface-coordinate limb of a GOES-R full disk. East/west use the
/// equatorial tangent; north/south use the ellipsoid's geodetic polar tangent,
/// which is about 0.029 degrees farther from nadir and therefore controls fit.
fn goes_full_disk_geometry(
    platform: &str,
    exact_center: Option<(f32, f32)>,
) -> Option<GoesFullDiskGeometry> {
    let normalized_platform = platform.trim().to_ascii_lowercase();
    if !matches!(
        normalized_platform.as_str(),
        "g16" | "g17" | "g18" | "g19" | "goes16" | "goes17" | "goes18" | "goes19"
    ) {
        return None;
    }

    let nominal_lon = sat_window::goes_nominal_sub_lon_deg(&normalized_platform);
    let (center_lat_deg, center_lon_deg) = exact_center
        .map(|(lat, lon)| (f64::from(lat), normalize_longitude_f64(f64::from(lon))))
        .filter(|(lat, lon)| {
            lat.is_finite()
                && lon.is_finite()
                && lat.abs() <= 5.0
                && longitude_delta_degrees(*lon, nominal_lon) <= 10.0
        })
        .unwrap_or((0.0, nominal_lon));

    let satellite_radius_m = GOES_NOMINAL_PERSPECTIVE_HEIGHT_M + GOES_NOMINAL_SEMI_MAJOR_M;
    let equatorial_arc_deg = (GOES_NOMINAL_SEMI_MAJOR_M / satellite_radius_m)
        .acos()
        .to_degrees();

    // Ellipsoid tangent in the satellite/earth-center meridian. Convert the
    // tangent point from geocentric Cartesian coordinates to geodetic latitude
    // because BowEcho's map coordinates are geodetic lat/lon.
    let tangent_x_m = GOES_NOMINAL_SEMI_MAJOR_M.powi(2) / satellite_radius_m;
    let tangent_z_m = GOES_NOMINAL_SEMI_MINOR_M
        * (1.0 - tangent_x_m.powi(2) / GOES_NOMINAL_SEMI_MAJOR_M.powi(2)).sqrt();
    let polar_arc_deg = (GOES_NOMINAL_SEMI_MAJOR_M.powi(2) * tangent_z_m)
        .atan2(GOES_NOMINAL_SEMI_MINOR_M.powi(2) * tangent_x_m)
        .to_degrees();
    let max_limb_arc_deg = equatorial_arc_deg.max(polar_arc_deg);

    Some(GoesFullDiskGeometry {
        center_lat_deg,
        center_lon_deg,
        east_limb_lon_deg: normalize_longitude_f64(center_lon_deg + equatorial_arc_deg),
        west_limb_lon_deg: normalize_longitude_f64(center_lon_deg - equatorial_arc_deg),
        north_limb_lat_deg: (center_lat_deg + polar_arc_deg).min(90.0),
        south_limb_lat_deg: (center_lat_deg - polar_arc_deg).max(-90.0),
        max_limb_arc_deg,
    })
}

/// Fit the complete ABI earth limb inside the shorter axis of the actual map
/// pane. The returned scale is points/degree, matching BowEcho's AEQD map.
fn goes_full_disk_map_fit(
    platform: &str,
    exact_center: Option<(f32, f32)>,
    pane_size: egui::Vec2,
) -> Option<GoesFullDiskMapFit> {
    if !pane_size.is_finite() || pane_size.x <= 0.0 || pane_size.y <= 0.0 {
        return None;
    }
    let geometry = goes_full_disk_geometry(platform, exact_center)?;
    let short_axis = pane_size.x.min(pane_size.y);
    let usable_diameter = short_axis - 2.0 * GOES_FULL_DISK_FIT_MARGIN_POINTS;
    let map_scale = usable_diameter / (2.0 * geometry.max_limb_arc_deg as f32);
    if !map_scale.is_finite() || map_scale < GOES_FULL_DISK_FIT_MIN_SCALE {
        return None;
    }
    Some(GoesFullDiskMapFit {
        center_lat_deg: geometry.center_lat_deg as f32,
        center_lon_deg: geometry.center_lon_deg as f32,
        map_scale,
        limb_radius_points: geometry.max_limb_arc_deg as f32 * map_scale,
    })
}

fn satellite_full_disk_platform(layer: &SatMapLayer) -> Option<&str> {
    if let Some(platform) = layer
        .native
        .as_ref()
        .and_then(|native| native.goes_full_disk_platform())
    {
        return Some(platform);
    }

    // Compatibility for old local `.rws` Full Disk frames which predate the
    // exact native archive. A focused `fulldisk_win…` preview is a crop and
    // must not advertise itself as the whole disk.
    let model = layer.key.model.trim().to_ascii_lowercase();
    let run = layer.key.run.trim().to_ascii_lowercase();
    let is_goes = matches!(model.as_str(), "g16" | "g17" | "g18" | "g19");
    let is_whole_disk = run.starts_with("fulldisk_") && !run.starts_with("fulldisk_win");
    (is_goes && is_whole_disk).then_some(layer.key.model.as_str())
}

fn radar_map_primary_pane_size(layout: PanelLayout, map_rect: egui::Rect) -> Option<egui::Vec2> {
    if !map_rect.is_finite() || !map_rect.is_positive() {
        return None;
    }
    if layout == PanelLayout::One {
        return Some(map_rect.size());
    }
    pane_cell_rects(layout, map_rect, 2.0)
        .first()
        .map(egui::Rect::size)
        .filter(|size| size.is_finite())
}

/// Preserve the GOES panel's provider/detail choices when turning its
/// single-band follow spec into a one-shot RGB request. The old inline
/// struct update silently fell back to `GoesCompositeSpec::default()` for
/// `downsample`, so changing Detail had no effect on Load RGB.
fn goes_composite_spec_from_follow(
    base: &rw_ui::SatFollowSpec,
    style: String,
    window: Option<sat_window::SatNativeWindow>,
) -> sat_worker::GoesCompositeSpec {
    sat_worker::GoesCompositeSpec {
        satellite: base.satellite.clone(),
        sector: base.sector.clone(),
        style,
        downsample: base.downsample,
        window,
        ..sat_worker::GoesCompositeSpec::default()
    }
}

/// Project one texel-grid vertex from the AEQD viewport where the stale
/// satellite raster was rendered into the current AEQD viewport. Repeating
/// this across a small mesh keeps the raster attached to geography while the
/// exact background rerender catches up; a single translated rectangle drifts
/// sideways because two AEQD view centers are not related by an affine shift.
fn stale_sat_texture_vertex(
    rect: egui::Rect,
    texture_pts: egui::Vec2,
    rendered: &ModelLayerView,
    current: &ModelLayerView,
    u: f32,
    v: f32,
) -> Option<egui::Pos2> {
    if !rect.is_finite()
        || !texture_pts.is_finite()
        || texture_pts.x <= 0.0
        || texture_pts.y <= 0.0
        || !rendered.center_lat.is_finite()
        || !rendered.center_lon.is_finite()
        || !rendered.map_scale.is_finite()
        || rendered.map_scale <= 0.0
        || !current.center_lat.is_finite()
        || !current.center_lon.is_finite()
        || !current.map_scale.is_finite()
        || current.map_scale <= 0.0
    {
        return None;
    }
    let rendered_km_per_pt = 111.32 / f64::from(rendered.map_scale);
    let east_km = f64::from((u - 0.5) * texture_pts.x) * rendered_km_per_pt;
    let north_km = f64::from((0.5 - v) * texture_pts.y) * rendered_km_per_pt;
    let (lat, lon) = aeqd_inverse_km(
        f64::from(rendered.center_lat),
        f64::from(rendered.center_lon),
        east_km,
        north_km,
    );
    let (current_east_km, current_north_km) = aeqd_forward_km(
        f64::from(current.center_lat),
        f64::from(current.center_lon),
        lat,
        lon,
    );
    let current_pts_per_km = f64::from(current.map_scale) / 111.32;
    let point = egui::pos2(
        rect.center().x + (current_east_km * current_pts_per_km) as f32,
        rect.center().y - (current_north_km * current_pts_per_km) as f32,
    );
    (point.x.is_finite() && point.y.is_finite()).then_some(point)
}

fn stale_sat_texture_mesh(
    texture_id: egui::TextureId,
    rect: egui::Rect,
    texture_pts: egui::Vec2,
    rendered: &ModelLayerView,
    current: &ModelLayerView,
    tint: egui::Color32,
) -> Option<egui::epaint::Mesh> {
    let n = SAT_STALE_DRAPE_GRID;
    let mut mesh = egui::epaint::Mesh::with_texture(texture_id);
    mesh.vertices.reserve((n + 1) * (n + 1));
    mesh.indices.reserve(n * n * 6);
    for j in 0..=n {
        for i in 0..=n {
            let u = i as f32 / n as f32;
            let v = j as f32 / n as f32;
            mesh.vertices.push(egui::epaint::Vertex {
                pos: stale_sat_texture_vertex(rect, texture_pts, rendered, current, u, v)?,
                uv: egui::pos2(u, v),
                color: tint,
            });
        }
    }
    let stride = (n + 1) as u32;
    for j in 0..n as u32 {
        for i in 0..n as u32 {
            let a = j * stride + i;
            let (b, c, d) = (a + 1, a + stride, a + stride + 1);
            mesh.add_triangle(a, c, b);
            mesh.add_triangle(b, c, d);
        }
    }
    Some(mesh)
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
        let remote = self.app_settings.community_cache.enabled.then(|| {
            sat_worker::RemoteSatelliteBootstrap {
                settings: self.app_settings.community_cache.clone(),
                cache_root: settings::community_cache_dir(),
            }
        });
        let notify = ctx.clone();
        let worker = sat_worker::SatWorker::spawn_with_remote(store, remote, move || {
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

    /// Reset all displayed satellite identity after a provider/product control
    /// changes, then rebuild the saved-loop picker. Keeping the prior map drape
    /// here lets a sparse MTG-lightning raster masquerade as newly selected GOES
    /// or Geo Colour imagery; the normal spec-change clear also invalidates any
    /// in-flight map render from that old product.
    fn refresh_satellite_catalog_for_provider_change(&mut self) {
        self.clear_satellite_display_for_spec_change();
        if let Some(worker) = &self.sat {
            worker.send(sat_worker::SatRequest::Scan);
        }
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
        // This action can originate in Layers while the Satellite pane is
        // closed, so the provider-controls change detector never sees it.
        // Reset the old run/drape explicitly before the MTG response arrives.
        self.refresh_satellite_catalog_for_provider_change();
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
        let mut catalog_filter_changed = false;
        if let Some(source) = selected_source
            && source != self.satellite_source
        {
            self.satellite_source = source;
            self.app_settings.satellite_source = self.satellite_source.slug().to_owned();
            self.mark_app_settings_dirty();
            catalog_filter_changed = true;
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
                if let Some(usage) = self.sat_storage_usage {
                    let preview_cap_per_channel = self.sat_panel.spec().max_bytes();
                    ui.label(
                        egui::RichText::new(satellite_storage_usage_text(
                            usage,
                            preview_cap_per_channel,
                        ))
                            .small()
                            .strong(),
                    )
                    .on_hover_text(
                        "Exact native NetCDF sources power zoom-dependent map tiles and selected RGB overviews. Compact .rws derivatives power scalar-channel loops; neither number is mislabeled as the other.",
                    );
                    ui.weak(
                        "The transfer list and its frames counter are component-channel preview writes. The saved loop exposes only complete product scan minutes.",
                    );
                }
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
                let band_before = self.himawari_band;
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
                catalog_filter_changed |= self.himawari_band != band_before;
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
                let product_before = self.eumetsat_product.clone();
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
                catalog_filter_changed |= self.eumetsat_product != product_before;

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

        if catalog_filter_changed {
            self.refresh_satellite_catalog_for_provider_change();
        }

        panel_kit::subgroup(ui, "RGB / MTG focused crop", |_ui| {});
        let mut window_changed = false;
        ui.horizontal_wrapped(|ui| {
            window_changed |= ui
                .checkbox(&mut self.sat_window_enabled, "Use focused crop")
                .on_hover_text(
                    "Use the saved center and size for the next provider RGB/MTG request. GOES/Himawari crops decode at instrument-native resolution; EUMETView requests a high-resolution geolocated crop.",
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
                self.sat_window_enabled = true;
                window_changed = true;
            }
        });
        ui.weak(match (self.satellite_source, self.sat_window_enabled) {
            (SatelliteSource::Goes, true) => {
                "Applies on the next Load RGB. A native focused crop overrides GOES Detail."
            }
            (SatelliteSource::Goes, false) => {
                "Focused crop is off. GOES Detail now applies to the next Load RGB."
            }
            (SatelliteSource::Himawari, true) => {
                "Applies on the next Load RGB. A native focused crop overrides the selected region/full-disk scope."
            }
            (SatelliteSource::Himawari, false) => {
                "Focused crop is off. The selected Himawari region/full-disk scope applies to the next Load RGB."
            }
            (SatelliteSource::Meteosat, true) => {
                "Applies on the next Meteosat Latest or Load loop request."
            }
            (SatelliteSource::Meteosat, false) => {
                "Focused crop is off. Meteosat requests use their normal provider extent."
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
                    goes_composite_spec_from_follow(&base, style, window),
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
            let mut fit_full_disk_on_map = false;
            let frame_available = self.sat_last_frame.is_some();
            let native_plot_available = self
                .sat_last_frame
                .as_ref()
                .is_some_and(|(key, _)| !satellite_run_key_is_remote(key));
            let full_disk_identity = self.sat_layer.as_ref().and_then(|layer| {
                let platform = satellite_full_disk_platform(layer)?.to_owned();
                let exact_center = layer
                    .native
                    .as_ref()
                    .and_then(|native| native.coverage_center());
                Some((platform, exact_center))
            });
            let map_pane_size = self
                .media
                .last_map_rect
                .and_then(|rect| radar_map_primary_pane_size(self.grid_layout, rect));
            let full_disk_fit = full_disk_identity.as_ref().zip(map_pane_size).and_then(
                |((platform, exact_center), pane_size)| {
                    goes_full_disk_map_fit(platform, *exact_center, pane_size)
                },
            );
            let center_action_available = self
                .sat_layer
                .as_ref()
                .zip(self.sat_layer_texture.as_ref())
                .is_some_and(|(layer, (_, generation, _, has_visible_pixels, _, _))| {
                    layer.generation == *generation
                        && !*has_visible_pixels
                        && (layer
                            .native
                            .as_ref()
                            .is_some_and(|native| native.coverage_center().is_some())
                            || layer.preview.as_ref().is_some_and(|preview| {
                                preview
                                    .lut
                                    .lookup(self.map_center_lat, self.map_center_lon)
                                    .is_none()
                            }))
                })
                && full_disk_identity.is_none();
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
                    .add_enabled(native_plot_available, egui::Button::new("Native plot"))
                    .on_hover_text(
                        if native_plot_available {
                            "Render the selected satellite frame through BowEcho's georeferenced native plotter. Scalar bands retain raw values/units; RGB composites retain their baked colors."
                        } else {
                            "rw-server satellite tiles are display-only and do not expose native science values for the plotter."
                        },
                    )
                    .clicked()
                {
                    plot_request = self.sat_last_frame.clone();
                }
                if full_disk_identity.is_some()
                    && ui
                        .add_enabled(
                            full_disk_fit.is_some(),
                            egui::Button::new("Fit full disk on map"),
                        )
                        .on_hover_text(if full_disk_fit.is_some() {
                            "Center the radar map on the exact GOES subpoint (or the satellite's nominal operational slot for rw-server imagery) and zoom so the complete ABI earth limb fits inside the primary map pane."
                        } else {
                            "The current radar-map pane is too small to fit the complete ABI earth limb safely."
                        })
                        .clicked()
                {
                    fit_full_disk_on_map = true;
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
            if fit_full_disk_on_map && let Some(fit) = full_disk_fit {
                self.clear_camera_follow_targets();
                self.center_map_on(fit.center_lat_deg, fit.center_lon_deg);
                // This is the sole intentional path below MIN_MAP_SCALE. The
                // ordinary wheel/global clamp remains 7 px/degree; lowering it
                // globally reintroduces near-antipode AEQD basemap smearing.
                self.map_scale = fit.map_scale;
                self.status = format!(
                    "GOES Full Disk fitted to map at {:.2} px/degree",
                    fit.map_scale
                );
                ui.ctx().request_repaint();
            }
            if center_on_satellite {
                let target = self.sat_layer.as_ref().and_then(|layer| {
                    layer
                        .native
                        .as_ref()
                        .and_then(|native| native.coverage_center())
                        .or_else(|| {
                            let preview = layer.preview.as_ref()?;
                            satellite_visible_coverage_point(
                                preview.image.as_ref(),
                                preview.grid.as_ref(),
                                preview.flip_rows,
                            )
                        })
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
        if self
            .sat_last_frame
            .as_ref()
            .is_some_and(|(key, _)| satellite_run_key_is_remote(key))
        {
            ui.weak(
                "This rw-server frame is already rendered into exact tiles; local IR enhancement changes apply to local/native frames only.",
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
                if selected_goes_player_product(self.sat_panel.spec()).is_none() {
                    for (key, hhmm) in sat_enhancement_refresh_frames(
                        &self.sat_run_listings,
                        self.sat_player.selected_run(),
                        self.sat_last_frame.as_ref(),
                    ) {
                        sat.send(sat_worker::SatRequest::LoadFrame {
                            key,
                            hhmm,
                            native_product: None,
                        });
                    }
                }
            }
            // The map layer recolors by ITS OWN identity (layer, else the
            // in-flight/queued request): timeline follow can point the map
            // at a different scan than the player, and keying this off
            // `sat_last_frame` left the map on the old palette then.
            if let Some((key, hhmm)) = self
                .sat_map_recolor_target()
                .filter(|(key, _)| !satellite_run_key_is_remote(key))
            {
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
        self.sat_storage_usage = None;
        self.sat_legacy_frames.clear();
        self.sat_run_listings.clear();
        self.sat_player.set_runs(Vec::new());
        self.sat_layer = None;
        self.sat_layer_texture = None;
        self.sat_layer_build_rx = None;
        self.cancel_sat_layer_render();
        self.reset_sat_layer_render_backoff();
        self.sat_map_inflight = None;
        self.sat_map_pending = None;
        self.sat_layer_generation = self.sat_layer_generation.wrapping_add(1);
        // `sat_lut_cache` deliberately survives: entries are keyed by grid
        // CONTENT (sha256), so a spec change can never make them wrong —
        // and toggling back to a spec whose grid was already indexed
        // reinstalls the layer without the multi-second LUT rebuild.
    }

    /// Stop a detached viewport raster at its next bounded checkpoint. Merely
    /// dropping the result receiver leaves native NetCDF reads (and remote
    /// HTTP tile requests) running, which made rapid scrub/pan pile up stale
    /// workers behind the current frame.
    fn cancel_sat_layer_render(&mut self) {
        if let Some(cancel) = self.sat_layer_render_cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.sat_layer_render_rx = None;
        self.sat_layer_render_generation = None;
        self.sat_layer_render_view = None;
    }

    fn finish_sat_layer_render(&mut self) {
        self.sat_layer_render_cancel = None;
        self.sat_layer_render_rx = None;
        self.sat_layer_render_generation = None;
        self.sat_layer_render_view = None;
    }

    fn reset_sat_layer_render_backoff(&mut self) {
        self.sat_layer_render_retry_after = None;
        self.sat_layer_render_failures = 0;
    }

    fn defer_failed_sat_layer_render(&mut self, ctx: &egui::Context) {
        self.sat_layer_render_failures = self.sat_layer_render_failures.saturating_add(1);
        let shift = u32::from(self.sat_layer_render_failures.saturating_sub(1).min(4));
        let seconds = (SAT_RENDER_RETRY_BASE.as_secs() << shift).min(SAT_RENDER_RETRY_MAX_SECS);
        let delay = Duration::from_secs(seconds);
        self.sat_layer_render_retry_after = Some(Instant::now() + delay);
        ctx.request_repaint_after(delay);
    }

    pub(crate) fn satellite_run_key_matches_current_spec(&self, key: &rw_ui::SatRunKey) -> bool {
        // A one-shot RGB/IR ingest (notably a hurricane-card full-disk
        // request) intentionally may not match the GOES live-follow panel's
        // sector. Once the worker explicitly selects it, keep that exact run
        // admissible until the user selects another run or changes the spec.
        if self
            .sat_last_frame
            .as_ref()
            .is_some_and(|(selected, _)| selected == key)
            || self
                .sat_player
                .selected_run()
                .is_some_and(|selected| selected == key)
        {
            return true;
        }
        match self.satellite_source {
            SatelliteSource::Goes => {
                satellite_run_key_matches_goes_spec(key, self.sat_panel.spec())
            }
            SatelliteSource::Himawari => {
                satellite_run_key_matches_himawari(key, self.himawari_band)
            }
            SatelliteSource::Meteosat => satellite_run_key_matches_meteosat(
                key,
                eumetsat::MtgProduct::parse(&self.eumetsat_product).unwrap_or_default(),
            ),
        }
    }

    pub(crate) fn satellite_runs_for_current_spec(
        &self,
        runs: Vec<rw_ui::SatRunListing>,
    ) -> Vec<rw_ui::SatRunListing> {
        let admitted = self.sat_last_frame.as_ref().map(|(key, _)| key);
        let selected = self.sat_player.selected_run();
        runs.into_iter()
            .filter(|run| {
                admitted.is_some_and(|key| key == &run.key)
                    || selected.is_some_and(|key| key == &run.key)
                    || self.satellite_run_key_matches_current_spec(&run.key)
            })
            .map(|mut run| {
                if self.satellite_source == SatelliteSource::Goes
                    && !satellite_run_key_is_remote(&run.key)
                    && !satellite_run_key_is_composite(&run.key)
                    && let Some(product) = selected_goes_player_product(self.sat_panel.spec())
                {
                    run.title = local_goes_product_run_title(&run.key, product);
                }
                run
            })
            .collect()
    }

    /// The player request matching the current GOES product. Baked RGB,
    /// SimSat, Himawari, Meteosat, and ordinary scalar runs keep their stored
    /// frame path. A local multi-channel GOES carrier instead asks the worker
    /// for the exact retained-native product overview.
    fn satellite_player_frame_request(
        &self,
        key: rw_ui::SatRunKey,
        hhmm: u16,
    ) -> sat_worker::SatRequest {
        let native_product = (self.satellite_source == SatelliteSource::Goes
            && !satellite_run_key_is_composite(&key)
            && self.satellite_run_key_matches_current_spec(&key))
        .then(|| selected_goes_player_product(self.sat_panel.spec()))
        .flatten()
        .map(|product| product.slug());
        sat_worker::SatRequest::LoadFrame {
            key,
            hhmm,
            native_product,
        }
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
        let native_product = (self.satellite_source == SatelliteSource::Goes)
            .then(|| self.sat_panel.spec().layer.clone());
        sat.send(sat_worker::SatRequest::LoadFrameForMap {
            key: key.clone(),
            hhmm,
            native_product,
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
                    // Starting a live follow is an explicit request to view
                    // the selected satellite product, not merely cache it in
                    // the background. Keep the radar-map layer attached to
                    // the player just like the one-shot "Load live loop" and
                    // RGB actions do. Previously this route left the default
                    // `sat_map_follow = false`, so polling could download and
                    // publish hundreds of megabytes of healthy native Full
                    // Disk frames while nothing was ever requested for the
                    // radar map.
                    self.sat_map_follow = true;
                    if let Some((key, hhmm)) = self.sat_last_frame.clone()
                        && !self.satellite_map_frame_current_or_scheduled(&key, hhmm)
                    {
                        self.request_sat_map_frame(key, hhmm);
                    }
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
        // A native Full Disk player can request the selected overview plus
        // neighboring prefetch frames in one UI pass. Those are expensive
        // whole-product renders. Admit every authoritative selection first
        // so its lightweight tiled map request reaches the worker ahead of
        // that preview/prefetch batch instead of sitting behind tens of
        // seconds of unrelated overview work.
        let mut deferred_player_events = Vec::new();
        for event in player_events {
            match event {
                rw_ui::SatPlayerEvent::FrameSelected { key, hhmm } => {
                    self.sat_last_frame = Some((key.clone(), hhmm));
                    if self.sat_map_follow
                        && !self.satellite_map_frame_current_or_scheduled(&key, hhmm)
                    {
                        self.request_sat_map_frame(key, hhmm);
                    }
                }
                other => deferred_player_events.push(other),
            }
        }
        for event in deferred_player_events {
            match event {
                rw_ui::SatPlayerEvent::FrameWanted { key, hhmm } => {
                    let request = self.satellite_player_frame_request(key, hhmm);
                    if let Some(sat) = &self.sat {
                        sat.send(request);
                    }
                }
                rw_ui::SatPlayerEvent::RefreshRequested => {
                    if let Some(sat) = &self.sat {
                        sat.send(sat_worker::SatRequest::Scan);
                    }
                }
                rw_ui::SatPlayerEvent::FrameSelected { .. } => unreachable!(
                    "authoritative satellite selections are handled before preview prefetch"
                ),
            }
        }
    }

    /// Admit a completed player texture load without changing the displayed
    /// playhead identity. SatPlayer requests the current frame plus prefetch
    /// neighbors; only `FrameSelected` (or an explicit `SelectFrame` response)
    /// is authoritative for `sat_last_frame` and map-follow.
    pub(crate) fn cache_loaded_satellite_frame(
        &mut self,
        key: rw_ui::SatRunKey,
        hhmm: u16,
        legacy: bool,
        frame: rw_ui::SatFrameImage,
    ) -> bool {
        if frame.key != key
            || frame.hhmm != hhmm
            || !self.satellite_run_key_matches_current_spec(&key)
        {
            return false;
        }
        if legacy {
            self.sat_legacy_frames.insert((key, hhmm));
        } else {
            self.sat_legacy_frames.remove(&(key, hhmm));
        }
        let (frame, resized) = sat_player_frame_within_texture_limit(frame);
        if let Some((old_size, new_size)) = resized {
            self.sat_panel.apply_note(format!(
                "preview downsampled {}x{} -> {}x{} for GPU texture limit",
                old_size[0], old_size[1], new_size[0], new_size[1]
            ));
        }
        self.sat_player.set_frame(frame);
        true
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
                sat_worker::SatResponse::RemoteCatalogOptions {
                    satellites,
                    sectors,
                    layers,
                } => {
                    self.sat_panel.set_satellite_options(satellites);
                    self.sat_panel.set_sector_options(sectors);
                    self.sat_panel.set_layer_options(layers);
                }
                sat_worker::SatResponse::Runs(runs) => {
                    let runs = self.satellite_runs_for_current_spec(runs);
                    self.sat_run_listings = runs.clone();
                    self.sat_player.set_runs(runs);
                }
                sat_worker::SatResponse::IngestReady { runs, key, hhmm } => {
                    // This is an explicit one-shot product result, not an
                    // unsolicited cache run. Admit its exact identity before
                    // applying the normal follow-spec filter so a tropical
                    // full-disk/window request can display while the panel is
                    // still configured for (for example) GOES-East CONUS.
                    self.sat_last_frame = Some((key.clone(), hhmm));
                    let runs = self.satellite_runs_for_current_spec(runs);
                    self.sat_run_listings = runs.clone();
                    self.sat_player.set_runs(runs);
                    self.sat_player.select_frame(key.clone(), hhmm);
                    if self.sat_map_follow
                        && !self.satellite_map_frame_current_or_scheduled(&key, hhmm)
                    {
                        self.request_sat_map_frame(key.clone(), hhmm);
                    }
                    // SatPlayer owns preview requests and marks them pending
                    // on its next UI pass. Sending one directly here made the
                    // same native RGB overview render twice, because the
                    // player could not see that host-side request.
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
                sat_worker::SatResponse::DownloadProgress {
                    id,
                    received_bytes,
                    total_bytes,
                } => {
                    self.sat_panel
                        .apply_download_progress(&id, received_bytes, total_bytes);
                    self.status = match total_bytes.filter(|total| *total > 0) {
                        Some(total) => format!(
                            "Satellite: downloading {} / {} ({:.0}%)",
                            rw_ui::format_bytes(received_bytes),
                            rw_ui::format_bytes(total),
                            received_bytes as f64 * 100.0 / total as f64,
                        ),
                        None => format!(
                            "Satellite: downloading {}",
                            rw_ui::format_bytes(received_bytes)
                        ),
                    };
                }
                sat_worker::SatResponse::DownloadDone { id, ms, cache_hit } => {
                    self.status = format!(
                        "Satellite: download complete in {ms} ms{} · decoding / storing",
                        if cache_hit { " · cache hit" } else { "" }
                    );
                    self.sat_panel.apply_download_done(&id, ms, cache_hit);
                }
                sat_worker::SatResponse::NativeFrameUpdated {
                    key,
                    hhmm,
                    committed_channel,
                } => {
                    self.status = format!(
                        "Satellite: native {key} {hhmm:04}Z C{committed_channel:02} retained at full source resolution"
                    );
                    self.sat_panel.apply_note(format!(
                        "native {key} {hhmm:04}Z C{committed_channel:02} retained at full source resolution"
                    ));
                    if let Some(sat) = &self.sat {
                        sat.send(satellite_native_frame_refresh_request(
                            key,
                            hhmm,
                            self.satellite_source,
                            self.sat_panel.spec(),
                        ));
                    }
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
                        "Satellite: compact preview stored {run}/t{hhmm:04} · {} in {encode_ms} ms",
                        rw_ui::format_bytes(bytes)
                    );
                    self.sat_panel.apply_frame_written(
                        &id,
                        format!("preview {run}"),
                        hhmm,
                        bytes,
                        encode_ms,
                    );
                    let selected_product = (self.satellite_source == SatelliteSource::Goes)
                        .then(|| selected_goes_player_product(self.sat_panel.spec()))
                        .flatten();
                    // Named multi-channel products refresh at the preceding
                    // NativeFrameUpdated durable boundary. Waiting for (or
                    // duplicating work after) this optional derivative would
                    // make a preview failure hide a healthy native product.
                    if selected_product.is_none()
                        && let Some(sat) = &self.sat
                    {
                        sat.send(satellite_frame_written_refresh_request(
                            model,
                            run,
                            hhmm,
                            select_live_run,
                            selected_product,
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
                sat_worker::SatResponse::StorageUsage(usage) => {
                    self.sat_storage_usage = Some(usage);
                }
                sat_worker::SatResponse::SelectFrame { key, hhmm } => {
                    // SimSat is an external producer rather than a provider
                    // tab. Its atomic ScanAndSelect response is therefore the
                    // authority that admits the just-written run. Ordinary
                    // provider selections must still match the active source
                    // controls so a stale response cannot put MTG Lightning
                    // back under GOES/Geo Colour labels.
                    let explicit_simsat = key.model == "simsat";
                    if !explicit_simsat && !self.satellite_run_key_matches_current_spec(&key) {
                        continue;
                    }
                    let needs_catalog_rescan =
                        explicit_simsat && !self.sat_run_listings.iter().any(|run| run.key == key);
                    self.sat_last_frame = Some((key.clone(), hhmm));
                    if needs_catalog_rescan && let Some(sat) = &self.sat {
                        // The Scan half of ScanAndSelect arrived before this
                        // explicit identity was admitted and was correctly
                        // source-filtered. Re-scan now so the player owns the
                        // new run before its colored frame lands.
                        sat.send(sat_worker::SatRequest::Scan);
                    }
                    self.sat_player.select_frame(key.clone(), hhmm);
                    if self.sat_map_follow
                        && !self.satellite_map_frame_current_or_scheduled(&key, hhmm)
                    {
                        self.request_sat_map_frame(key.clone(), hhmm);
                    }
                    // The player requests the selected texture (and bounded
                    // prefetch neighbors) itself. Avoid a duplicate expensive
                    // full-product overview render here.
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
                        self.cache_loaded_satellite_frame(key, hhmm, legacy, frame);
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
        let sat_worker::SatMapFrame {
            key,
            hhmm,
            native,
            remote,
            preview,
        } = frame;
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
        let generation = self.sat_layer_generation + 1;

        if let Some(native) = native {
            self.sat_layer_generation = generation;
            self.sat_layer = Some(SatMapLayer {
                key,
                hhmm,
                native: Some(Arc::new(sat_native_map::NativeTileRenderer::new(native))),
                preview: None,
                opacity: initial_opacity,
                visible: initial_visible,
                generation,
            });
            self.cancel_sat_layer_render();
            self.reset_sat_layer_render_backoff();
            self.status = format!("Satellite map: {hhmm:04}Z · native source ready");
            return;
        }

        if let Some(remote) = remote {
            self.sat_layer_generation = generation;
            self.sat_layer = Some(SatMapLayer {
                key,
                hhmm,
                native: Some(Arc::new(sat_native_map::NativeTileRenderer::new_remote(
                    remote,
                ))),
                preview: None,
                opacity: initial_opacity,
                visible: initial_visible,
                generation,
            });
            self.cancel_sat_layer_render();
            self.reset_sat_layer_render_backoff();
            self.status = format!("Satellite map: {hhmm:04}Z · native server tiles ready");
            return;
        }

        let Some(preview) = preview else {
            self.status = format!("Satellite map {key} {hhmm:04}Z has no renderable source");
            return;
        };
        let nx = preview.grid.nx;
        let ny = preview.grid.ny;
        if let Some(existing) = &self.sat_layer
            && existing.key == key
            && let Some(existing_preview) = &existing.preview
            && existing_preview.nx == nx
            && existing_preview.ny == ny
            && existing_preview.flip_rows == preview.flip_rows
        {
            self.sat_layer_generation = generation;
            self.sat_layer = Some(SatMapLayer {
                key,
                hhmm,
                native: None,
                preview: Some(SatMapPreviewLayer {
                    image: Arc::new(preview.image),
                    grid: Arc::clone(&existing_preview.grid),
                    lut: Arc::clone(&existing_preview.lut),
                    nx,
                    ny,
                    flip_rows: existing_preview.flip_rows,
                }),
                opacity: existing.opacity,
                visible: existing.visible,
                generation,
            });
            // Keep the previous map texture visible until the next frame's
            // viewport render is ready. Clearing here makes playback blink
            // blank between frames, especially now that sat overlays render
            // at full viewport resolution.
            self.cancel_sat_layer_render();
            self.reset_sat_layer_render_backoff();
            self.status = format!("Satellite map: {hhmm:04}Z");
            return;
        }
        // Different run key, but a grid we have already indexed (successive
        // runs of a product write bit-identical grids; spec toggles come
        // back to the same grid): reuse the cached LUT synchronously
        // instead of parking playback behind a multi-second rebuild.
        if let Some(lut) = self.sat_lut_cache_get(&preview.grid.hash, nx, ny) {
            self.sat_layer_generation = generation;
            self.sat_layer = Some(SatMapLayer {
                key,
                hhmm,
                native: None,
                preview: Some(SatMapPreviewLayer {
                    image: Arc::new(preview.image),
                    grid: preview.grid,
                    lut,
                    nx,
                    ny,
                    flip_rows: preview.flip_rows,
                }),
                opacity: initial_opacity,
                visible: initial_visible,
                generation,
            });
            // As above: the previous texture stays up as a placeholder until
            // this generation's viewport render lands.
            self.cancel_sat_layer_render();
            self.reset_sat_layer_render_backoff();
            self.status = format!("Satellite map: {hhmm:04}Z");
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.sat_layer_build_rx = Some(receiver);
        self.status = format!("Building satellite layer {key} {hhmm:04}Z");
        let ctx = ctx.clone();
        thread::spawn(move || {
            let layer = model_layer::InverseLut::build_with_shape(
                &preview.grid.lat,
                &preview.grid.lon,
                nx,
                ny,
            )
            .map(|lut| SatMapLayer {
                key,
                hhmm,
                native: None,
                preview: Some(SatMapPreviewLayer {
                    image: Arc::new(preview.image),
                    grid: Arc::clone(&preview.grid),
                    lut: Arc::new(lut),
                    nx,
                    ny,
                    flip_rows: preview.flip_rows,
                }),
                opacity: initial_opacity,
                visible: initial_visible,
                generation,
            });
            let _ = sender.send(layer);
            ctx.request_repaint();
        });
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
                        if let Some(preview) = &layer.preview {
                            self.sat_lut_cache_insert(
                                preview.grid.hash.clone(),
                                preview.nx,
                                preview.ny,
                                Arc::clone(&preview.lut),
                            );
                        }
                        self.sat_layer_generation = layer.generation;
                        self.sat_layer = Some(layer);
                        self.cancel_sat_layer_render();
                        self.reset_sat_layer_render_backoff();
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
                Ok(rendered) => {
                    self.finish_sat_layer_render();
                    if let Some((key, hhmm, outside_coverage, can_center, remote_native)) = self
                        .sat_layer
                        .as_ref()
                        .filter(|layer| layer.generation == rendered.generation)
                        .map(|layer| {
                            let outside_coverage = layer.preview.as_ref().is_some_and(|preview| {
                                preview
                                    .lut
                                    .lookup(rendered.view.center_lat, rendered.view.center_lon)
                                    .is_none()
                            });
                            let can_center = layer
                                .native
                                .as_ref()
                                .is_some_and(|native| native.coverage_center().is_some());
                            let remote_native = layer
                                .native
                                .as_ref()
                                .is_some_and(|native| native.is_remote());
                            (
                                layer.key.clone(),
                                layer.hhmm,
                                outside_coverage,
                                can_center,
                                remote_native,
                            )
                        })
                    {
                        match rendered.image {
                            Ok(image) => {
                                self.reset_sat_layer_render_backoff();
                                let has_visible_pixels =
                                    satellite_render_has_visible_pixels(&image);
                                let texture = ctx.load_texture(
                                    "sat-layer",
                                    image,
                                    egui::TextureOptions::LINEAR,
                                );
                                self.sat_layer_texture = Some((
                                    texture,
                                    rendered.generation,
                                    rendered.view,
                                    has_visible_pixels,
                                    key,
                                    hhmm,
                                ));
                                self.status = if let Some(error) = rendered.native_error {
                                    format!(
                                        "Satellite native map unavailable; using bounded preview: {error}"
                                    )
                                } else if has_visible_pixels && rendered.native {
                                    format!("Satellite map: {hhmm:04}Z · native resolution")
                                } else if has_visible_pixels {
                                    format!("Satellite map: {hhmm:04}Z · bounded preview")
                                } else if outside_coverage {
                                    "Satellite map: current view is outside this sector — use Center on satellite coverage".to_owned()
                                } else if rendered.native && can_center {
                                    "Satellite map: no native pixels in this view — use Center on satellite coverage or try IR at night".to_owned()
                                } else if rendered.native && remote_native {
                                    "Satellite map: no native pixels in this view — rw-server v3 has no per-frame coverage center; pan toward the sector or try IR at night".to_owned()
                                } else if rendered.native {
                                    "Satellite map: no native pixels in this view — this frame has no coverage-center metadata; pan toward the sector or try IR at night".to_owned()
                                } else {
                                    "Satellite map: frame is transparent in this view — try an IR layer at night".to_owned()
                                };
                            }
                            Err(error) => {
                                // A failed exact tile is not a transparent
                                // frame. Keep the last known-good texture and
                                // retry after bounded backoff.
                                self.status = self
                                    .sat_layer_texture
                                    .as_ref()
                                    .filter(|(_, generation, _, _, _, _)| {
                                        *generation != rendered.generation
                                    })
                                    .map(|(_, _, _, _, stale_key, stale_hhmm)| {
                                        format!(
                                            "Satellite map {key} {hhmm:04}Z failed; showing {stale_key} {stale_hhmm:04}Z STALE: {error}"
                                        )
                                    })
                                    .unwrap_or_else(|| {
                                        format!(
                                            "Satellite native map failed; keeping the last valid map: {error}"
                                        )
                                    });
                                self.defer_failed_sat_layer_render(ctx);
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.finish_sat_layer_render();
                    if self.sat_layer.is_some() {
                        self.status =
                            "Satellite map renderer stopped; keeping the last valid map".to_owned();
                        self.defer_failed_sat_layer_render(ctx);
                    }
                }
            }
        }
    }

    /// Draw the satellite layer (world-anchored; renders at viewport
    /// resolution on a background thread, exactly like the model layer).
    pub(crate) fn draw_sat_layer(&mut self, painter: &egui::Painter, rect: egui::Rect) {
        let Some((generation, visible)) = self
            .sat_layer
            .as_ref()
            .map(|layer| (layer.generation, layer.visible))
        else {
            self.cancel_sat_layer_render();
            return;
        };
        if !visible {
            self.cancel_sat_layer_render();
            return;
        }
        let view = self.model_layer_current_view();
        let render_is_obsolete = self.sat_layer_render_rx.is_some()
            && (self.sat_layer_render_generation != Some(generation)
                || self
                    .sat_layer_render_view
                    .as_ref()
                    .is_none_or(|requested| model_layer_view_needs_rerender(requested, &view)));
        if render_is_obsolete {
            // Latest viewport/frame wins. The detached worker observes the
            // token between rows/tiles and stops before burning the whole old
            // native render.
            self.cancel_sat_layer_render();
        }
        let current = self
            .sat_layer_texture
            .as_ref()
            .filter(|(_, rendered_generation, _, _, _, _)| *rendered_generation == generation);
        let needs_render = current
            .map(|(_, _, have, _, _, _)| model_layer_view_needs_rerender(have, &view))
            .unwrap_or(true);
        let defer_render = map_layer_rerender_deferred(painter.ctx());
        let now = Instant::now();
        let retry_ready = self
            .sat_layer_render_retry_after
            .is_none_or(|retry_after| now >= retry_after);
        if !retry_ready && let Some(retry_after) = self.sat_layer_render_retry_after {
            painter
                .ctx()
                .request_repaint_after(retry_after.saturating_duration_since(now));
        }
        if needs_render && !defer_render && retry_ready && self.sat_layer_render_rx.is_none() {
            let Some((native, preview)) = self.sat_layer.as_ref().map(|layer| {
                let native = layer.native.as_ref().map(Arc::clone);
                let preview = layer.preview.as_ref().map(|preview| {
                    (
                        Arc::clone(&preview.image),
                        Arc::clone(&preview.grid),
                        Arc::clone(&preview.lut),
                        preview.nx,
                        preview.ny,
                        preview.flip_rows,
                    )
                });
                (native, preview)
            }) else {
                return;
            };
            let (sender, receiver) = mpsc::channel();
            self.sat_layer_render_rx = Some(receiver);
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            self.sat_layer_render_cancel = Some(Arc::clone(&cancel));
            self.sat_layer_render_generation = Some(generation);
            self.sat_layer_render_view = Some(view);
            let render_view = view;
            let center_lat = view.center_lat as f64;
            let center_lon = view.center_lon as f64;
            let km_per_pt = 111.32 / view.map_scale as f64;
            let (w_pts, h_pts) = (rect.width() as f64, rect.height() as f64);
            let ctx = painter.ctx().clone();
            thread::spawn(move || {
                let (w, h) = model_layer_render_dimensions(w_pts, h_pts, render_view.map_scale);
                let native_result = native.as_ref().map(|native| {
                    native.render_aeqd_cancellable(
                        center_lat,
                        center_lon,
                        render_view.map_scale,
                        w_pts,
                        h_pts,
                        w,
                        h,
                        cancel.as_ref(),
                    )
                });
                let (image, rendered_native, native_error) = match native_result {
                    Some(Ok(image)) => (Ok(image), true, None),
                    Some(Err(error)) => {
                        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                            return;
                        }
                        if let Some(preview) = preview.as_ref() {
                            (
                                render_sat_preview_aeqd(
                                    preview,
                                    center_lat,
                                    center_lon,
                                    km_per_pt,
                                    w_pts,
                                    h_pts,
                                    w,
                                    h,
                                    cancel.as_ref(),
                                ),
                                false,
                                Some(error),
                            )
                        } else {
                            (Err(error.clone()), true, Some(error))
                        }
                    }
                    None => {
                        let image = preview.as_ref().map_or_else(
                            || Err("satellite frame has no renderable map source".to_owned()),
                            |preview| {
                                render_sat_preview_aeqd(
                                    preview,
                                    center_lat,
                                    center_lon,
                                    km_per_pt,
                                    w_pts,
                                    h_pts,
                                    w,
                                    h,
                                    cancel.as_ref(),
                                )
                            },
                        );
                        (image, false, None)
                    }
                };
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let _ = sender.send(SatLayerRender {
                    generation,
                    view: render_view,
                    image,
                    native: rendered_native,
                    native_error,
                });
                ctx.request_repaint();
            });
        }
        let Some(layer) = &self.sat_layer else {
            return;
        };
        if let Some((
            texture,
            rendered_generation,
            rendered,
            has_visible_pixels,
            rendered_key,
            rendered_hhmm,
        )) = &self.sat_layer_texture
        {
            let stale_identity = *rendered_generation != layer.generation
                || rendered_key != &layer.key
                || *rendered_hhmm != layer.hhmm;
            let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
            let tint = egui::Color32::from_white_alpha((layer.opacity * 255.0) as u8);
            if model_layer_view_needs_rerender(rendered, &view) {
                // Mid-gesture (owner report: "every time we zoom in the sat
                // image disappears until we stop"): keep painting the last
                // rendered raster through a small georeferenced drape. A
                // single translated rectangle drifts sideways because the
                // old and new AEQD views are not affinely related.
                if let Some(mesh) = stale_sat_texture_mesh(
                    texture.id(),
                    rect,
                    texture.size_vec2(),
                    rendered,
                    &view,
                    tint,
                ) {
                    painter.add(egui::Shape::mesh(mesh));
                } else {
                    // Degenerate projection inputs are rare; retain the old
                    // affine bridge as a safe visual fallback.
                    let anchored =
                        anchored_sat_texture_rect(rect, texture.size_vec2(), rendered, &view);
                    if anchored.is_finite() && anchored.intersects(rect) {
                        painter.image(texture.id(), anchored, uv, tint);
                    }
                }
            } else {
                painter.image(texture.id(), rect, uv, tint);
            }
            if stale_identity {
                draw_satellite_stale_notice(
                    painter,
                    rect,
                    rendered_key,
                    *rendered_hhmm,
                    &layer.key,
                    layer.hhmm,
                );
            } else if !has_visible_pixels && !model_layer_view_needs_rerender(rendered, &view) {
                draw_satellite_no_visible_notice(
                    painter,
                    rect,
                    layer.preview.as_ref().is_some_and(|preview| {
                        preview
                            .lut
                            .lookup(view.center_lat, view.center_lon)
                            .is_none()
                    }),
                    layer
                        .native
                        .as_ref()
                        .is_some_and(|native| native.coverage_center().is_some()),
                    layer
                        .native
                        .as_ref()
                        .is_some_and(|native| native.is_remote()),
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

fn satellite_storage_usage_text(
    usage: sat_worker::SatStorageUsage,
    preview_cap_per_channel: Option<u64>,
) -> String {
    let lower_bound = if usage.inventory_complete {
        ""
    } else {
        "at least "
    };
    let native_cap = usage
        .native_cap_bytes
        .map(|bytes| format!(" · {} combined native cap", rw_ui::format_bytes(bytes)))
        .unwrap_or_else(|| " · native archive unbounded".to_owned());
    let preview_cap = preview_cap_per_channel
        .map(|bytes| format!(" · {} cap/channel", rw_ui::format_bytes(bytes)))
        .unwrap_or_else(|| " · preview cache unbounded".to_owned());
    format!(
        "Exact native: {lower_bound}{} · {} source scan minute(s), partial allowed / {} channel source(s){native_cap}  |  Preview cache: {lower_bound}{} · {} channel frame(s){preview_cap}",
        rw_ui::format_bytes(usage.native_bytes),
        usage.native_unique_scans,
        usage.native_channel_sources,
        rw_ui::format_bytes(usage.preview_bytes),
        usage.preview_channel_frames,
    )
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

fn selected_goes_player_product(spec: &rw_ui::SatFollowSpec) -> Option<rw_sat::GoesAbiProduct> {
    rw_sat::GoesAbiProduct::parse(&spec.layer)
        .filter(|product| product.required_channels().len() > 1)
}

/// A named multi-channel product owns one timeline, not one timeline per
/// component channel. Its base-channel run is the stable carrier because it
/// has the product's native output grid (C02 for both GeoColor products).
fn goes_player_run_filters(spec: &rw_ui::SatFollowSpec) -> Result<(String, Vec<String>), String> {
    let (model, mut prefixes) = sat_worker::run_filters_for_spec(spec)?;
    if let Some(product) = selected_goes_player_product(spec) {
        let suffix = format!("_c{:02}", product.base_channel());
        prefixes.retain(|prefix| prefix.ends_with(&suffix));
        if prefixes.len() != 1 {
            return Err(format!(
                "{} has no unique base-channel timeline",
                product.title()
            ));
        }
    }
    Ok((model, prefixes))
}

fn satellite_run_key_matches_goes_spec(
    key: &rw_ui::SatRunKey,
    spec: &rw_ui::SatFollowSpec,
) -> bool {
    let Ok((model, prefixes)) = goes_player_run_filters(spec) else {
        return false;
    };
    if !satellite_run_key_matches_resolved_spec(key, &model, &prefixes) {
        return false;
    }
    let Some(product) = selected_goes_player_product(spec) else {
        return true;
    };
    let slug = product.slug();
    key.run.contains(&format!("_rwproduct_{slug}_"))
        || key.run.contains(&format!("_rwserver_{slug}_"))
}

fn local_goes_product_run_title(key: &rw_ui::SatRunKey, product: rw_sat::GoesAbiProduct) -> String {
    let base_marker = format!("_c{:02}_", product.base_channel());
    let sector = key
        .run
        .split_once(&base_marker)
        .map(|(sector, _)| sector)
        .unwrap_or("goes");
    let sector = match sector {
        "fulldisk" => "Full disk".to_owned(),
        "conus" => "CONUS".to_owned(),
        "meso1" => "Meso 1".to_owned(),
        "meso2" => "Meso 2".to_owned(),
        other => other.replace('_', " "),
    };
    let day = key
        .run
        .split('_')
        .find(|token| token.len() == 8 && token.bytes().all(|byte| byte.is_ascii_digit()))
        .map(|day| format!("{}-{}-{}", &day[..4], &day[4..6], &day[6..]))
        .unwrap_or_else(|| "stored".to_owned());
    format!("{} · {sector} {} · {day}", key.model, product.title())
}

fn satellite_frame_written_refresh_request(
    model: String,
    run: String,
    hhmm: u16,
    select_live_run: bool,
    selected_product: Option<rw_sat::GoesAbiProduct>,
) -> sat_worker::SatRequest {
    if select_live_run && matches!(model.as_str(), "g16" | "g17" | "g18" | "g19") {
        let key = rw_ui::SatRunKey { model, run };
        if let Some(product) = selected_product {
            sat_worker::SatRequest::ScanAndSelectNativeProduct {
                key,
                hhmm,
                product: product.slug(),
            }
        } else {
            sat_worker::SatRequest::ScanAndSelect { key, hhmm }
        }
    } else {
        sat_worker::SatRequest::Scan
    }
}

fn satellite_native_frame_refresh_request(
    key: rw_ui::SatRunKey,
    hhmm: u16,
    source: SatelliteSource,
    spec: &rw_ui::SatFollowSpec,
) -> sat_worker::SatRequest {
    let Some(product) = (source == SatelliteSource::Goes)
        .then(|| selected_goes_player_product(spec))
        .flatten()
    else {
        return sat_worker::SatRequest::Scan;
    };
    let Ok((model, prefixes)) = sat_worker::run_filters_for_spec(spec) else {
        return sat_worker::SatRequest::Scan;
    };
    let committed_channel_matches = product
        .required_channels()
        .iter()
        .any(|channel| run_has_band_token(&key.run, 'c', *channel));
    if committed_channel_matches && satellite_run_key_matches_resolved_spec(&key, &model, &prefixes)
    {
        sat_worker::SatRequest::ScanAndSelectNativeProduct {
            key,
            hhmm,
            product: product.slug(),
        }
    } else {
        sat_worker::SatRequest::Scan
    }
}

fn draw_satellite_no_visible_notice(
    painter: &egui::Painter,
    rect: egui::Rect,
    outside_coverage: bool,
    can_center: bool,
    remote_native: bool,
) {
    let text = if outside_coverage {
        "No visible satellite pixels in this map view\nCurrent map is outside this satellite sector\nUse Center on satellite coverage in the Satellite panel"
    } else if remote_native && !can_center {
        "No visible satellite pixels in this map view\nrw-server v3 has no per-frame coverage center\nPan toward the sector or try an IR layer at night"
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

fn draw_satellite_stale_notice(
    painter: &egui::Painter,
    rect: egui::Rect,
    displayed_key: &rw_ui::SatRunKey,
    displayed_hhmm: u16,
    requested_key: &rw_ui::SatRunKey,
    requested_hhmm: u16,
) {
    let text = format!(
        "STALE SATELLITE FRAME\nShowing {displayed_key} {displayed_hhmm:04}Z\nLoading {requested_key} {requested_hhmm:04}Z"
    );
    let style = painter.ctx().global_style();
    let visuals = &style.visuals;
    let galley = painter.layout(
        text,
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(255, 214, 92),
        (rect.width() - 32.0).max(120.0),
    );
    let size = galley.size() + egui::vec2(20.0, 14.0);
    let card = egui::Rect::from_min_size(rect.min + egui::vec2(12.0, 12.0), size);
    painter.rect_filled(card, 5.0, visuals.window_fill());
    painter.rect_stroke(
        card,
        5.0,
        egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 214, 92)),
        egui::StrokeKind::Inside,
    );
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
    if satellite_run_key_is_remote(key) {
        // Server tiles already carry their rendered product. Re-requesting a
        // whole remote day cannot recolor it and can waste hundreds of tile
        // requests/quota while filling the bounded preview lane.
        return Vec::new();
    }
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

pub(crate) fn satellite_run_key_is_remote(key: &rw_ui::SatRunKey) -> bool {
    key.run.contains("_rwserver_")
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

/// Match one stored scalar-band run against the resolved GOES live-follow
/// spec. One-shot RGB products are admitted by exact identity only after the
/// ingest response selects them; admitting every cached composite here lets a
/// newer but unrelated RGB run replace the selected scalar product on scan.
fn satellite_run_key_matches_resolved_spec(
    key: &rw_ui::SatRunKey,
    model: &str,
    prefixes: &[String],
) -> bool {
    key.model == model
        && prefixes
            .iter()
            .any(|prefix| key.run.starts_with(prefix.as_str()))
}

fn run_has_band_token(run: &str, prefix: char, band: u8) -> bool {
    let token = format!("{prefix}{band:02}");
    run.split('_').any(|candidate| candidate == token)
}

fn satellite_run_key_matches_himawari(key: &rw_ui::SatRunKey, band: u8) -> bool {
    if !matches!(key.model.as_str(), "h8" | "h9") {
        return false;
    }
    let band = band.clamp(7, 16);
    run_has_band_token(&key.run, 'c', band) || run_has_band_token(&key.run, 'b', band)
}

fn satellite_run_key_matches_meteosat(
    key: &rw_ui::SatRunKey,
    product: eumetsat::MtgProduct,
) -> bool {
    key.model == "mtg_i1" && key.run.contains(&format!("_rgb_wms_{}_", product.slug()))
}

/// Whether a store run contains baked RGB planes rather than one scalar band.
/// Timeline/map consumers use this content-identity test independently of the
/// stricter provider/product catalog filter above.
// Retained as a narrow whole-app regression-test seam.
#[allow(dead_code)]
pub(crate) fn satellite_run_key_is_composite(key: &rw_ui::SatRunKey) -> bool {
    key.run.contains("_rgb_")
}

/// Normalize a dated run name to its reusable product/view family.
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

type SatPreviewRenderInput = (
    Arc<egui::ColorImage>,
    Arc<rw_store::grid::GridFile>,
    Arc<model_layer::InverseLut>,
    usize,
    usize,
    bool,
);

#[allow(clippy::too_many_arguments)]
fn render_sat_preview_aeqd(
    source: &SatPreviewRenderInput,
    center_lat: f64,
    center_lon: f64,
    km_per_point: f64,
    logical_width: f64,
    logical_height: f64,
    raster_width: usize,
    raster_height: usize,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<egui::ColorImage, String> {
    let (image, grid, lut, nx, ny, flip_rows) = source;
    let mut pixels = vec![egui::Color32::TRANSPARENT; raster_width * raster_height];
    let sample_ctx = SatMapSampleCtx {
        image: image.as_ref(),
        grid: grid.as_ref(),
        nx: *nx,
        ny: *ny,
        flip_rows: *flip_rows,
    };
    for (row, pixel_row) in pixels.chunks_mut(raster_width).enumerate() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("satellite preview render cancelled".to_owned());
        }
        for (column, pixel) in pixel_row.iter_mut().enumerate() {
            let logical_x = (column as f64 + 0.5) * logical_width / raster_width as f64;
            let logical_y = (row as f64 + 0.5) * logical_height / raster_height as f64;
            let east_km = (logical_x - logical_width * 0.5) * km_per_point;
            let north_km = (logical_height * 0.5 - logical_y) * km_per_point;
            let (latitude, longitude) = aeqd_inverse_km(center_lat, center_lon, east_km, north_km);
            let Some(nearest) = lut.lookup(latitude as f32, longitude as f32) else {
                continue;
            };
            let color =
                sample_sat_map_color(&sample_ctx, nearest, latitude as f32, longitude as f32);
            if color.a() > 0 {
                *pixel = color;
            }
        }
    }
    Ok(egui::ColorImage {
        size: [raster_width, raster_height],
        source_size: egui::vec2(logical_width as f32, logical_height as f32),
        pixels,
    })
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
    fn goes_full_disk_geometry_uses_correct_g18_and_g19_slots_and_limbs() {
        let east = goes_full_disk_geometry("g19", None).expect("G19 geometry");
        assert!((east.center_lat_deg - 0.0).abs() < 1.0e-9);
        assert!((east.center_lon_deg - (-75.2)).abs() < 1.0e-9);
        assert!((east.east_limb_lon_deg - 6.099_516_7).abs() < 1.0e-5);
        assert!((east.west_limb_lon_deg - (-156.499_516_7)).abs() < 1.0e-5);
        assert!((east.north_limb_lat_deg - 81.328_243_6).abs() < 1.0e-5);
        assert!((east.south_limb_lat_deg - (-81.328_243_6)).abs() < 1.0e-5);

        let west = goes_full_disk_geometry("goes18", None).expect("G18 geometry");
        assert!((west.center_lon_deg - (-137.0)).abs() < 1.0e-9);
        assert!((west.east_limb_lon_deg - (-55.700_483_3)).abs() < 1.0e-5);
        assert!((west.west_limb_lon_deg - 141.700_483_3).abs() < 1.0e-5);
        assert!((west.max_limb_arc_deg - 81.328_243_6).abs() < 1.0e-5);
    }

    #[test]
    fn goes_full_disk_fit_uses_actual_short_pane_axis_with_margin() {
        for pane_size in [
            egui::vec2(500.0, 400.0),
            egui::vec2(800.0, 600.0),
            egui::vec2(1_024.0, 768.0),
        ] {
            let fit = goes_full_disk_map_fit("g19", None, pane_size).expect("fit");
            let occupied = fit.limb_radius_points * 2.0 + GOES_FULL_DISK_FIT_MARGIN_POINTS * 2.0;
            assert!((occupied - pane_size.x.min(pane_size.y)).abs() < 0.01);
            assert!(
                fit.map_scale < MIN_MAP_SCALE,
                "sub-1138-point panes need the explicit Full Disk scale path: {fit:?}"
            );
        }
    }

    #[test]
    fn goes_full_disk_fit_prefers_sane_exact_center_and_rejects_bad_metadata() {
        let exact = goes_full_disk_map_fit("g19", Some((0.125, -75.05)), egui::vec2(900.0, 700.0))
            .expect("exact-center fit");
        assert!((exact.center_lat_deg - 0.125).abs() < f32::EPSILON);
        assert!((exact.center_lon_deg - (-75.05)).abs() < f32::EPSILON);

        let fallback = goes_full_disk_map_fit("g18", Some((45.0, 10.0)), egui::vec2(900.0, 700.0))
            .expect("nominal-center fallback");
        assert_eq!(fallback.center_lat_deg, 0.0);
        assert_eq!(fallback.center_lon_deg, -137.0);
        assert!(goes_full_disk_map_fit("h9", None, egui::vec2(900.0, 700.0)).is_none());
    }

    #[test]
    fn satellite_storage_text_never_calls_a_preview_the_native_source() {
        let exact = satellite_storage_usage_text(
            sat_worker::SatStorageUsage {
                preview_bytes: 17 * 1024 * 1024,
                preview_channel_frames: 1,
                native_bytes: 414 * 1024 * 1024,
                native_unique_scans: 1,
                native_channel_sources: 1,
                native_cap_bytes: Some(9 * 1024 * 1024 * 1024),
                inventory_complete: true,
            },
            Some(3 * 1024 * 1024 * 1024),
        );
        assert!(exact.contains("Exact native: 414.0 MB"), "{exact}");
        assert!(exact.contains("Preview cache: 17.0 MB"), "{exact}");
        assert!(exact.contains("9.00 GB combined native cap"), "{exact}");
        assert!(exact.contains("3.00 GB cap/channel"), "{exact}");
        assert!(exact.contains("partial allowed"), "{exact}");

        let bounded = satellite_storage_usage_text(
            sat_worker::SatStorageUsage {
                inventory_complete: false,
                ..Default::default()
            },
            None,
        );
        assert!(bounded.matches("at least").count() >= 2, "{bounded}");
    }

    #[test]
    fn goes_rgb_request_preserves_the_selected_detail() {
        let base = rw_ui::SatFollowSpec {
            satellite: "goes18".to_owned(),
            sector: "fulldisk".to_owned(),
            downsample: 7,
            ..rw_ui::SatFollowSpec::default()
        };
        let window = sat_window::SatNativeWindow {
            center_lat_deg: 22.0,
            center_lon_deg: -105.0,
            size_km: 600.0,
        };

        let request =
            goes_composite_spec_from_follow(&base, "natural_color".to_owned(), Some(window));

        assert_eq!(request.satellite, "goes18");
        assert_eq!(request.sector, "fulldisk");
        assert_eq!(request.downsample, 7);
        assert_eq!(request.window, Some(window));
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
    fn feedback_v03412_goes_catalog_rejects_cross_product_provider_and_sector_runs() {
        let model = "g19";
        let prefixes = vec!["conus_c13_".to_owned()];
        let key = |model: &str, run: &str| rw_ui::SatRunKey {
            model: model.to_owned(),
            run: run.to_owned(),
        };

        assert!(satellite_run_key_matches_resolved_spec(
            &key("g19", "conus_c13_20260713"),
            model,
            &prefixes,
        ));
        assert!(
            !satellite_run_key_matches_resolved_spec(
                &key("g19", "conus_rgb_natural_color_20260713"),
                model,
                &prefixes,
            ),
            "one-shot RGB is admitted only after its exact ingest response"
        );
        assert!(
            !satellite_run_key_matches_resolved_spec(
                &key("g19", "conus_win295n954w600_rgb_natural_color_20260713"),
                model,
                &prefixes,
            ),
            "a focused one-shot RGB is not the selected live-follow product"
        );
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
        assert!(
            !satellite_run_key_matches_resolved_spec(
                &key("h9", "fulldisk_rgb_true_color_20260713"),
                model,
                &prefixes,
            ),
            "a provider tab must not inherit unrelated saved runs"
        );
    }

    #[test]
    fn feedback_v03412_non_goes_catalog_filters_keep_products_with_provider_controls() {
        let key = |model: &str, run: &str| rw_ui::SatRunKey {
            model: model.to_owned(),
            run: run.to_owned(),
        };

        assert!(satellite_run_key_matches_himawari(
            &key("h9", "fulldisk_c13_20260713"),
            13,
        ));
        assert!(
            !satellite_run_key_matches_himawari(&key("h9", "fulldisk_rgb_true_color_20260713"), 13,),
            "one-shot true color is admitted only after its exact ingest response"
        );
        assert!(
            !satellite_run_key_matches_himawari(
                &key("h9", "fulldisk_win150n1400e600_rgb_ir13_20260713"),
                13,
            ),
            "a focused one-shot IR run must not replace the full-disk scalar run"
        );
        assert!(!satellite_run_key_matches_himawari(
            &key("h9", "fulldisk_c08_20260713"),
            13,
        ));
        assert!(!satellite_run_key_matches_himawari(
            &key("g19", "fulldisk_c13_20260713"),
            13,
        ));

        assert!(satellite_run_key_matches_meteosat(
            &key("mtg_i1", "mtg_fd_rgb_wms_geo_colour_20260723"),
            eumetsat::MtgProduct::GeoColour,
        ));
        assert!(
            !satellite_run_key_matches_meteosat(
                &key("mtg_i1", "mtg_fd_rgb_wms_lightning_afa_20260723"),
                eumetsat::MtgProduct::GeoColour,
            ),
            "an old sparse lightning loop must never masquerade as Geo Colour"
        );
        assert!(!satellite_run_key_matches_meteosat(
            &key("g19", "conus_rgb_natural_color_20260723"),
            eumetsat::MtgProduct::GeoColour,
        ));
    }

    #[test]
    fn feedback_v03412_explicit_one_shot_run_survives_the_live_follow_filter() {
        let listing = |model: &str, run: &str| rw_ui::SatRunListing {
            key: rw_ui::SatRunKey {
                model: model.to_owned(),
                run: run.to_owned(),
            },
            title: run.to_owned(),
            nx: 100,
            ny: 100,
            frames: vec![1200],
        };
        let conus = listing("g19", "conus_c13_20260713");
        let tropical = listing("g19", "fulldisk_win227n875w1000_rgb_natural_color_20260713");
        let prefixes = vec!["conus_c13_".to_owned()];

        let normal = vec![conus.clone(), tropical.clone()]
            .into_iter()
            .filter(|run| satellite_run_key_matches_resolved_spec(&run.key, "g19", &prefixes))
            .collect::<Vec<_>>();
        assert_eq!(normal.len(), 1);
        assert_eq!(normal[0].key, conus.key);

        let admitted = &tropical.key;
        let forced = vec![conus, tropical.clone()]
            .into_iter()
            .filter(|run| {
                &run.key == admitted
                    || satellite_run_key_matches_resolved_spec(&run.key, "g19", &prefixes)
            })
            .collect::<Vec<_>>();
        assert_eq!(forced.len(), 2);
        assert!(forced.iter().any(|run| run.key == tropical.key));
    }

    #[test]
    fn stale_satellite_drape_round_trips_an_unchanged_view() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(300.0, 200.0));
        let view = ModelLayerView {
            center_lat: 39.0,
            center_lon: -95.0,
            map_scale: 100.0,
        };
        for (u, v, expected) in [
            (0.0, 0.0, rect.left_top()),
            (1.0, 0.0, rect.right_top()),
            (1.0, 1.0, rect.right_bottom()),
            (0.0, 1.0, rect.left_bottom()),
            (0.5, 0.5, rect.center()),
        ] {
            let actual = stale_sat_texture_vertex(rect, rect.size(), &view, &view, u, v)
                .expect("valid projection");
            assert!(
                actual.distance(expected) < 0.02,
                "({u}, {v}) landed at {actual:?}, expected {expected:?}"
            );
        }
    }

    #[test]
    fn stale_satellite_drape_reprojects_instead_of_sliding_as_one_rectangle() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0));
        let rendered = ModelLayerView {
            center_lat: 30.0,
            center_lon: -120.0,
            map_scale: 60.0,
        };
        let current = ModelLayerView {
            center_lat: 42.0,
            center_lon: -95.0,
            map_scale: 60.0,
        };
        let left = stale_sat_texture_vertex(rect, rect.size(), &rendered, &current, 0.0, 0.0)
            .expect("left");
        let middle = stale_sat_texture_vertex(rect, rect.size(), &rendered, &current, 0.5, 0.0)
            .expect("middle");
        let right = stale_sat_texture_vertex(rect, rect.size(), &rendered, &current, 1.0, 0.0)
            .expect("right");
        let affine_middle = egui::pos2((left.x + right.x) * 0.5, (left.y + right.y) * 0.5);

        assert!(
            middle.distance(affine_middle) > 0.1,
            "the curved AEQD edge must not collapse back to the old affine slide"
        );
    }

    #[test]
    fn live_goes_frame_refresh_scans_and_selects_the_just_written_run() {
        match satellite_frame_written_refresh_request(
            "g18".to_owned(),
            "conus_c01_20260713".to_owned(),
            1851,
            true,
            None,
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
                None,
            ),
            sat_worker::SatRequest::Scan
        ));
    }

    #[test]
    fn live_multichannel_product_refreshes_from_native_commit_not_preview_write() {
        let spec = rw_ui::SatFollowSpec {
            satellite: "goes19".to_owned(),
            sector: "fulldisk".to_owned(),
            layer: "open_geocolor_v1".to_owned(),
            ..rw_ui::SatFollowSpec::default()
        };
        match satellite_native_frame_refresh_request(
            rw_ui::SatRunKey {
                model: "g19".to_owned(),
                run: "fulldisk_c03_20260826".to_owned(),
            },
            1640,
            SatelliteSource::Goes,
            &spec,
        ) {
            sat_worker::SatRequest::ScanAndSelectNativeProduct {
                key,
                hhmm,
                product: selected,
            } => {
                assert_eq!(key.model, "g19");
                assert_eq!(key.run, "fulldisk_c03_20260826");
                assert_eq!(hhmm, 1640);
                assert_eq!(selected, "open_geocolor_v1");
            }
            other => panic!("expected strict native-product selection, got {other:?}"),
        }
        assert!(matches!(
            satellite_native_frame_refresh_request(
                rw_ui::SatRunKey {
                    model: "g18".to_owned(),
                    run: "fulldisk_c03_20260826".to_owned(),
                },
                1640,
                SatelliteSource::Goes,
                &spec,
            ),
            sat_worker::SatRequest::Scan
        ));
    }

    #[test]
    fn multichannel_product_exposes_one_product_named_carrier_timeline() {
        let spec = rw_ui::SatFollowSpec {
            satellite: "goes19".to_owned(),
            sector: "fulldisk".to_owned(),
            layer: "open_geocolor_v1".to_owned(),
            ..rw_ui::SatFollowSpec::default()
        };
        let (model, prefixes) = goes_player_run_filters(&spec).expect("player filter");
        assert_eq!(model, "g19");
        assert_eq!(prefixes, vec!["fulldisk_c02"]);

        let raw_key = rw_ui::SatRunKey {
            model: model.clone(),
            run: "fulldisk_c02_20260826".to_owned(),
        };
        assert!(
            !satellite_run_key_matches_goes_spec(&raw_key, &spec),
            "a raw C02 timeline must never masquerade as the selected product"
        );
        let key = rw_ui::SatRunKey {
            model,
            run: "fulldisk_c02_rwproduct_open_geocolor_v1_20260826".to_owned(),
        };
        assert!(satellite_run_key_matches_goes_spec(&key, &spec));
        let title = local_goes_product_run_title(&key, rw_sat::GoesAbiProduct::OpenGeoColorV1);
        assert!(title.contains("Full disk Open GeoColor Day v1"), "{title}");
        assert!(title.contains("2026-08-26"), "{title}");
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
