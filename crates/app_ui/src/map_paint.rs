//! Map-paint sub-move A (projection): the azimuthal-equidistant screen
//! projection primitives and the `GeoBounds` view-extent type, moved
//! VERBATIM out of `main.rs` (v0.30 decomposition, the final `map_paint`
//! extraction). These are called from the residual paint dispatch in
//! `main.rs` (`single_pane_canvas`/`grid_canvas` and the first `impl
//! ViewerApp` block) and from sibling feature modules (annotate, tropical,
//! tor_tracks, hazard_ui, event_explorer, gbvtd_retrieval, max_ref_swath,
//! overlays), so the moved items are promoted to `pub(crate)`.

use crate::*;

impl ViewerApp {
    /// Azimuthal-equidistant projection about the map center (north up):
    /// screen offsets are true great-circle kilometres, so range and azimuth
    /// are exact at the center and the frame matches the radar raster's
    /// planar ENU geometry (the equirectangular mapping it replaces skewed
    /// east-west distances away from the center latitude).
    pub(crate) fn lon_lat_to_screen(
        &self,
        rect: egui::Rect,
        longitude_deg: f32,
        latitude_deg: f32,
    ) -> egui::Pos2 {
        let (east_km, north_km) = aeqd_forward_km(
            self.map_center_lat as f64,
            self.map_center_lon as f64,
            latitude_deg as f64,
            longitude_deg as f64,
        );
        let px_per_km = self.map_scale / 111.32;
        egui::pos2(
            rect.center().x + east_km as f32 * px_per_km,
            rect.center().y - north_km as f32 * px_per_km,
        )
    }

    pub(crate) fn screen_to_lon_lat(&self, rect: egui::Rect, position: egui::Pos2) -> (f32, f32) {
        let km_per_px = 111.32 / self.map_scale;
        let east_km = (position.x - rect.center().x) * km_per_px;
        let north_km = (rect.center().y - position.y) * km_per_px;
        let (lat, lon) = aeqd_inverse_km(
            self.map_center_lat as f64,
            self.map_center_lon as f64,
            east_km as f64,
            north_km as f64,
        );
        (normalize_lon(lon as f32), lat as f32)
    }

    pub(crate) fn visible_geo_bounds(&self, rect: egui::Rect) -> GeoBounds {
        // Under AEQD the lat/lon extremes of the view sit on the EDGES, not
        // two corners (parallels bow poleward, meridians converge) — sample
        // four corners plus four edge midpoints (review finding F1).
        let samples = [
            rect.left_top(),
            rect.right_top(),
            rect.left_bottom(),
            rect.right_bottom(),
            egui::pos2(rect.center().x, rect.top()),
            egui::pos2(rect.center().x, rect.bottom()),
            egui::pos2(rect.left(), rect.center().y),
            egui::pos2(rect.right(), rect.center().y),
        ];
        let mut bounds = GeoBounds {
            west: f32::INFINITY,
            east: f32::NEG_INFINITY,
            south: f32::INFINITY,
            north: f32::NEG_INFINITY,
        };
        for sample in samples {
            let (lon, lat) = self.screen_to_lon_lat(rect, sample);
            bounds.west = bounds.west.min(lon);
            bounds.east = bounds.east.max(lon);
            bounds.south = bounds.south.min(lat);
            bounds.north = bounds.north.max(lat);
        }
        // If a pole is inside the view radius every longitude is visible.
        let km_per_px = 111.32 / self.map_scale;
        let view_radius_km = (rect.width().hypot(rect.height()) * 0.5 * km_per_px) as f64;
        const KM_PER_DEG: f64 = 111.32;
        let north_pole_km = (90.0 - self.map_center_lat as f64) * KM_PER_DEG;
        let south_pole_km = (90.0 + self.map_center_lat as f64) * KM_PER_DEG;
        if north_pole_km < view_radius_km || south_pole_km < view_radius_km {
            bounds.west = -180.0;
            bounds.east = 180.0;
        }
        bounds.south = bounds.south.clamp(-85.0, 85.0);
        bounds.north = bounds.north.clamp(-85.0, 85.0);
        bounds
    }

    /// Deviation of local "screen north" from straight up at a geo point —
    /// the AEQD meridian-convergence angle (radians, clockwise positive).
    /// The radar raster is planar ENU about the radar, so its quad is
    /// rotated by this angle to sit correctly in the AEQD frame (F2).
    pub(crate) fn aeqd_north_angle(
        &self,
        rect: egui::Rect,
        latitude_deg: f32,
        longitude_deg: f32,
    ) -> f32 {
        let base = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg);
        let north = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg + 0.05);
        let v = north - base;
        if v.length_sq() < 1e-12 {
            return 0.0;
        }
        v.x.atan2(-v.y)
    }

    pub(crate) fn clamp_map_center(&mut self) {
        self.map_center_lon = normalize_lon(self.map_center_lon);
        self.map_center_lat = self.map_center_lat.clamp(-85.0, 85.0);
    }

    pub(crate) fn lon_screen_scale(&self) -> f32 {
        self.map_center_lat.to_radians().cos().abs().max(0.02)
    }

    pub(crate) fn lon_pixels_per_degree(&self) -> f32 {
        self.map_scale * self.lon_screen_scale()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GeoBounds {
    pub(crate) west: f32,
    pub(crate) south: f32,
    pub(crate) east: f32,
    pub(crate) north: f32,
}

impl GeoBounds {
    pub(crate) fn expand(self, degrees: f32) -> Self {
        Self {
            west: self.west - degrees,
            south: self.south - degrees,
            east: self.east + degrees,
            north: self.north + degrees,
        }
    }

    pub(crate) fn intersect(self, other: Self) -> Option<Self> {
        let bounds = Self {
            west: self.west.max(other.west),
            south: self.south.max(other.south),
            east: self.east.min(other.east),
            north: self.north.min(other.north),
        };
        (bounds.west < bounds.east && bounds.south < bounds.north).then_some(bounds)
    }

    pub(crate) fn contains(self, longitude_deg: f32, latitude_deg: f32) -> bool {
        longitude_deg >= self.west
            && longitude_deg <= self.east
            && latitude_deg >= self.south
            && latitude_deg <= self.north
    }

    pub(crate) fn intersects_bbox(self, bbox: [f32; 4]) -> bool {
        bbox[2] >= self.west
            && bbox[0] <= self.east
            && bbox[3] >= self.south
            && bbox[1] <= self.north
    }
}

/// Map-paint sub-move B (chrome): the on-canvas overlays painted over the
/// radar/basemap layers — place-name/admin labels, graticule and range ring,
/// the color-scale legend, the LIVE/ARCHIVE/STALE mode chip, velocity-quality
/// tags, the center crosshair and Vrot measurement tool, the hover inspector
/// card + Field Loupe, and the cursor readout builders. Moved VERBATIM out of
/// `main.rs`; the entry-point painters are `pub(crate)` because the residual
/// paint dispatch (`single_pane_canvas`/`grid_canvas`), the first `impl
/// ViewerApp` block, sibling modules, and the test module call them.
impl ViewerApp {
    pub(crate) fn draw_center_crosshair(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !self.app_settings.show_center_crosshair {
            return;
        }
        let center = rect.center();
        let gap = 5.0;
        let arm = 18.0;
        let shadow = egui::Stroke::new(3.0, egui::Color32::from_black_alpha(180));
        let red = egui::Stroke::new(1.8, egui::Color32::from_rgb(255, 30, 30));
        let segments = [
            [
                center + egui::vec2(-arm, 0.0),
                center + egui::vec2(-gap, 0.0),
            ],
            [center + egui::vec2(gap, 0.0), center + egui::vec2(arm, 0.0)],
            [
                center + egui::vec2(0.0, -arm),
                center + egui::vec2(0.0, -gap),
            ],
            [center + egui::vec2(0.0, gap), center + egui::vec2(0.0, arm)],
        ];
        for segment in segments {
            painter.line_segment(segment, shadow);
            painter.line_segment(segment, red);
        }
    }

    /// GR2-style Vrot measurement overlay: two clicked gates (max inbound +
    /// max outbound), connecting line, and a card with
    /// Vrot = (|Vin| + |Vout|) / 2, couplet diameter, and beam height.
    pub(crate) fn draw_vrot_tool(&self, painter: &egui::Painter, rect: egui::Rect) {
        if self.vrot_points.is_empty() {
            return;
        }
        let positions: Vec<egui::Pos2> = self
            .vrot_points
            .iter()
            .map(|&(lon, lat, ..)| self.lon_lat_to_screen(rect, lon, lat))
            .collect();
        for (index, position) in positions.iter().enumerate() {
            let value = self.vrot_points[index].2;
            let color = if value < 0.0 {
                egui::Color32::from_rgb(80, 220, 120)
            } else {
                egui::Color32::from_rgb(240, 90, 80)
            };
            painter.circle_filled(*position, 4.0, color);
            painter.circle_stroke(*position, 4.0, egui::Stroke::new(1.2, egui::Color32::BLACK));
        }
        if self.vrot_points.len() == 2 {
            painter.line_segment(
                [positions[0], positions[1]],
                egui::Stroke::new(1.6, egui::Color32::from_rgb(245, 230, 120)),
            );
            let (lon_a, lat_a, v_a, h_a) = self.vrot_points[0];
            let (lon_b, lat_b, v_b, h_b) = self.vrot_points[1];
            let vrot_mps = (v_a.abs() + v_b.abs()) / 2.0;
            let (velocity_unit, velocity_scale) = self.vrot_display_unit();
            let diameter_km = haversine_km(lat_a, lon_a, lat_b, lon_b);
            let diameter_nm = diameter_km * 0.539_957;
            let height_kft = ((h_a + h_b) / 2.0) * 3.280_84 / 1000.0;
            let mid = egui::pos2(
                (positions[0].x + positions[1].x) / 2.0,
                (positions[0].y + positions[1].y) / 2.0,
            );
            let label = format!(
                "Vrot {:.0} kt · dia {:.1} nm · {:.1} kft",
                vrot_mps / velocity_scale,
                diameter_nm,
                height_kft
            );
            let label = label.replace(" kt", &format!(" {velocity_unit}"));
            draw_heavy_halo_text(
                painter,
                mid + egui::vec2(0.0, -14.0),
                egui::Align2::CENTER_BOTTOM,
                &label,
                egui::FontId::proportional(13.0),
                egui::Color32::from_rgb(250, 240, 180),
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 230),
            );
        } else {
            draw_halo_text(
                painter,
                positions[0] + egui::vec2(8.0, -8.0),
                egui::Align2::LEFT_BOTTOM,
                "click max outbound",
                egui::FontId::proportional(11.0),
                egui::Color32::from_rgb(245, 230, 120),
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 200),
            );
        }
    }

    /// "RAW VEL" tag under the mode chip whenever a velocity product renders
    /// WITHOUT dealiasing — folded gates read as opposite-direction flow, so
    /// raw mode must never be silent (operational safety).
    pub(crate) fn draw_raw_velocity_tag(&self, painter: &egui::Painter, rect: egui::Rect) {
        self.draw_velocity_quality_tag_for_product(
            painter,
            rect,
            &self.selected_product,
            34.0,
            self.primary_velocity_provider_id(),
        );
    }

    pub(crate) fn draw_raw_velocity_tag_for_product(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        product: &DisplayProduct,
        top_offset: f32,
    ) {
        if !product.is_signed_radial_velocity()
            || self.product_render_uses_dealiased_velocity(product)
            || self.volume.is_none()
        {
            return;
        }
        let pos = egui::pos2(rect.left() + 10.0, rect.top() + top_offset);
        let label = "RAW VEL — folds possible";
        let width = 16.0 + label.chars().count() as f32 * 7.2;
        let chip = egui::Rect::from_min_size(pos, egui::vec2(width, 20.0));
        painter.rect_filled(chip, 4.0, egui::Color32::from_rgb(120, 70, 20));
        painter.text(
            chip.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgb(248, 238, 220),
        );
    }

    /// True when the DISPLAYED cut's velocity feed carries no usable
    /// Nyquist, making the render2d dealiaser a pure pass-through there
    /// (JMA always — staggered PRF; occasional ODIM files missing
    /// `how/NI`). Drives the "no Nyquist" honesty tag for DVEL/DSRV.
    pub(crate) fn displayed_cut_dealias_skipped_no_nyquist(&self) -> bool {
        let Some(volume) = self.volume.as_ref() else {
            return false;
        };
        let Some(cut) = volume.cuts.get(self.selected_cut) else {
            return false;
        };
        let Some(grid) = cut.moments.get(&MomentType::Velocity) else {
            return false;
        };
        render2d::dealias_skipped_no_nyquist(cut, grid)
    }

    fn draw_velocity_quality_tag_for_product(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        product: &DisplayProduct,
        top_offset: f32,
        provider_id: Option<&str>,
    ) {
        if self.volume.is_none() {
            return;
        }
        let renders_dealiased = self.product_render_uses_dealiased_velocity(product);
        let Some(tag) = velocity_quality_tag(
            renders_dealiased,
            product.is_signed_radial_velocity(),
            renders_dealiased && self.displayed_cut_dealias_skipped_no_nyquist(),
            provider_id,
        ) else {
            return;
        };
        let (bg, fg) = match tag {
            VelocityQualityTag::DealiasedLocally => (
                egui::Color32::from_rgb(44, 78, 108),
                egui::Color32::from_rgb(228, 238, 248),
            ),
            // Warning palette: both mean "what you see is raw velocity".
            VelocityQualityTag::NoNyquistPassThrough | VelocityQualityTag::RawFoldsPossible => (
                egui::Color32::from_rgb(120, 70, 20),
                egui::Color32::from_rgb(248, 238, 220),
            ),
        };
        let label = tag.label();
        let pos = egui::pos2(rect.left() + 10.0, rect.top() + top_offset);
        let width = 16.0 + label.chars().count() as f32 * 7.2;
        let chip = egui::Rect::from_min_size(pos, egui::vec2(width, 20.0));
        painter.rect_filled(chip, 4.0, bg);
        painter.text(
            chip.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(12.0),
            fg,
        );
    }

    fn primary_velocity_provider_id(&self) -> Option<&str> {
        if !self.intl_source_owns_primary_display() {
            return None;
        }
        match &self.primary.feed {
            FeedSource::Live(SiteRef::Intl { provider_id, .. }) => Some(provider_id.as_str()),
            // Stage-(i) invariant: primary feed is CustomUrl or Live(Intl)
            // only; neither carries an intl velocity provider here.
            _ => None,
        }
    }

    /// Paint a LIVE / ARCHIVE / STALE mode chip top-left so a stale frame is
    /// never mistaken for live data (operational safety).
    pub(crate) fn draw_mode_chip(&self, painter: &egui::Painter, rect: egui::Rect) {
        let Some((label, bg, _)) = self.mode_chip_state() else {
            return;
        };
        let pos = egui::pos2(rect.left() + 10.0, rect.top() + 10.0);
        let width = 16.0 + label.chars().count() as f32 * 7.2;
        let chip = egui::Rect::from_min_size(pos, egui::vec2(width, 20.0));
        painter.rect_filled(chip, 4.0, bg);
        painter.text(
            chip.center(),
            egui::Align2::CENTER_CENTER,
            &label,
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgb(232, 236, 240),
        );
    }

    /// The live flag the PRIMARY chip truth uses for the current display
    /// owner. This is the ONE remaining bridge between the legacy
    /// `poll_active` field and the engine's `live.enabled` (see the
    /// `primary` field doc): an armed international owner's truth is the
    /// poll flag (no intl path sets the US auto-refresh switch); every
    /// other owner keeps the US/custom-URL flag, which stage (i) already
    /// mapped onto `primary.live.enabled`. Dies when the polled install
    /// path unifies (blocked on the class-(c) differential cells) and the
    /// two flags merge.
    pub(crate) fn primary_chip_live_flag(&self) -> bool {
        if self.intl_source_owns_primary_display() {
            self.intl_poll_owns_primary()
        } else {
            self.primary.live.enabled
        }
    }

    /// The PRIMARY mode chip, derived from the primary ENGINE's liveness
    /// (v0.29 Phase 4e stage (iii), spec §3): feed variant + live flag +
    /// NEWEST history frame age through
    /// [`ui_core::loop_engine::LoopEngine::liveness_with_live_flag`], so
    /// the chip can no longer disagree with engine state — an archive feed
    /// structurally cannot read LIVE (the R8 class). The international
    /// stale floor is now the CADENCE-AWARE `stale_floor_seconds` inside
    /// the engine derivation (spec §12 owner decision 3), superseding the
    /// flat `INTL_STALE_CHIP_FLOOR_SECONDS` call this fn used to make
    /// (same 1800 s value at the 60 s intl catalog cadence — labels are
    /// byte-identical).
    ///
    /// Deliberate normalize (pinned by the differential suite's
    /// `liveness_diff_displayed_vs_newest_age_is_the_spec_normalize`): the
    /// age is the NEWEST frame's, not the displayed frame's — browsing an
    /// old frame of a fresh live feed reads LIVE, and an archive loop's
    /// age readout follows the loop's newest scan.
    pub(crate) fn mode_chip_state(&self) -> Option<(String, egui::Color32, &'static str)> {
        let user_stale_chip_seconds = self.style_registry.radar_age().stale_chip_seconds;
        let liveness = self.primary.liveness_with_live_flag(
            Utc::now(),
            user_stale_chip_seconds,
            self.primary_chip_live_flag(),
        )?;
        let newest_time = self
            .primary
            .history
            .last()
            .map(|frame| frame.identity.scan_time_utc)?;
        Some(self.chip_for_liveness(liveness, newest_time))
    }

    /// PANE-side chip derivation with explicit liveness + stale-floor
    /// inputs: builds the same [`Liveness`] verdict from the DISPLAYED
    /// volume's age — inside a pane context swap `self.volume` is the
    /// pane's own volume, and the flag is the PANE's refresh reality
    /// passed by the caller — then renders through the ONE chip renderer
    /// so the strings cannot drift (census D11). The primary chip derives
    /// from the engine in [`Self::mode_chip_state`] instead. The floor:
    /// international feeds publish on 5-15 minute cadences, so a live intl
    /// chip only flags STALE past twice the slowest routine cadence (a
    /// user stale threshold above the floor still wins).
    pub(crate) fn mode_chip_state_with_live_and_stale_floor(
        &self,
        live: bool,
        stale_floor_seconds: i64,
    ) -> Option<(String, egui::Color32, &'static str)> {
        let volume = self.volume.as_ref()?;
        let aged_time = volume.volume_time.with_timezone(&Utc);
        let age_seconds = (Utc::now() - aged_time).num_seconds().max(0);
        let stale_chip_seconds = self
            .style_registry
            .radar_age()
            .stale_chip_seconds
            .max(0)
            .max(stale_floor_seconds);
        let liveness = if live {
            Liveness::Live {
                age_seconds,
                stale: age_seconds > stale_chip_seconds,
            }
        } else {
            Liveness::Archive { age_seconds }
        };
        Some(self.chip_for_liveness(liveness, aged_time))
    }

    /// The ONE mode-chip renderer over a [`Liveness`] verdict — every
    /// string here is greppable-identical to the pre-4e chips (census
    /// D11). `aged_time` is the frame the verdict's age was derived from
    /// (dated ARCHIVE chips print it).
    fn chip_for_liveness(
        &self,
        liveness: Liveness,
        aged_time: DateTime<Utc>,
    ) -> (String, egui::Color32, &'static str) {
        let age_style = self.style_registry.radar_age();
        match liveness {
            Liveness::Live { stale: false, .. } => (
                "● LIVE".to_owned(),
                style_color32(age_style.fresh_color),
                "LIVE",
            ),
            Liveness::Live {
                age_seconds,
                stale: true,
            } => (
                format!("● LIVE · STALE {}m", age_seconds / 60),
                style_color32(age_style.stale_color),
                "STALE",
            ),
            Liveness::Archive { age_seconds } => {
                let label = if age_seconds >= 24 * 60 * 60 {
                    format!("ARCHIVE · {}", aged_time.format("%Y-%m-%d %H:%MZ"))
                } else {
                    format!("ARCHIVE · {}m old", age_seconds / 60)
                };
                (label, style_color32(age_style.aging_color), "ARCH")
            }
        }
    }

    /// Paint an on-canvas color-scale legend for the active product, bottom-right.
    pub(crate) fn draw_colorbar(&self, painter: &egui::Painter, rect: egui::Rect) {
        if self.texture.is_none() {
            return;
        }
        self.draw_colorbar_for_product(painter, rect, &self.selected_product.clone());
    }

    /// The table the product actually renders with: the per-product
    /// palette override when one is set, else the family default.
    pub(crate) fn active_table_for_product(&self, product: &DisplayProduct) -> &ColorTable {
        self.palette_product_overrides
            .get(product.label())
            .unwrap_or_else(|| self.color_tables.for_family(product.color_family()))
    }

    pub(crate) fn vrot_display_unit(&self) -> (&'static str, f32) {
        let velocity_product = self.current_velocity_product();
        table_display_unit(
            self.active_table_for_product(&velocity_product),
            &velocity_product,
            self.units(),
        )
    }

    fn current_velocity_product(&self) -> DisplayProduct {
        if product_units(&self.selected_product) == "m/s" {
            self.selected_product.clone()
        } else {
            DisplayProduct::Moment(MomentType::Velocity)
        }
    }

    /// The Settings ▸ Display units footnote, present only when it applies
    /// (the current velocity product's table declares its own unit).
    pub(crate) fn velocity_units_note_for_settings(&self) -> Option<String> {
        let velocity_product = self.current_velocity_product();
        let table = self.active_table_for_product(&velocity_product);
        let unit = table_declared_velocity_unit(table, &velocity_product)?;
        Some(velocity_units_settings_note(unit, table.name()))
    }

    pub(crate) fn draw_colorbar_for_product(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        product: &DisplayProduct,
    ) {
        let render_tables = self.render_color_tables_for_product(product);
        let table = render_tables.for_family(product.color_family());
        let stops = table.stops();
        let (Some(first), Some(last)) = (stops.first(), stops.last()) else {
            return;
        };
        let (vmin, vmax) = (first.value, last.value);
        if vmax <= vmin {
            return;
        }

        let bar_w = 16.0;
        let margin = 12.0;
        let bar_h = (rect.height() * 0.42).clamp(120.0, 360.0);
        let x0 = rect.right() - margin - bar_w;
        let top = rect.bottom() - margin - bar_h;
        let bottom = top + bar_h;

        // backing panel (semi-transparent) behind labels + bar
        let panel = egui::Rect::from_min_max(
            // 34 = label gap (5) + widest tick text ("-30" ~24px) + pad.
            egui::pos2(x0 - 34.0, top - 20.0),
            egui::pos2(rect.right() - margin + 3.0, bottom + 6.0),
        );
        painter.rect_filled(
            panel,
            4.0,
            egui::Color32::from_rgba_unmultiplied(10, 12, 15, 200),
        );

        // gradient (top = vmax, bottom = vmin), ~1px steps
        let steps = bar_h.round().max(1.0) as usize;
        let step_h = bar_h / steps as f32;
        for i in 0..steps {
            let f = i as f32 / (steps.max(2) - 1) as f32;
            let v = vmax + (vmin - vmax) * f;
            let c = table.color_for_value(v);
            if c[3] == 0 {
                continue;
            }
            let y = top + bar_h * f;
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x0, y),
                    egui::pos2(x0 + bar_w, y + step_h + 1.0),
                ),
                0.0,
                egui::Color32::from_rgb(c[0], c[1], c[2]),
            );
        }

        // border (four segments — avoids StrokeKind API churn)
        let border = egui::Stroke::new(1.0, egui::Color32::from_gray(120));
        let (tl, tr) = (egui::pos2(x0, top), egui::pos2(x0 + bar_w, top));
        let (bl, br) = (egui::pos2(x0, bottom), egui::pos2(x0 + bar_w, bottom));
        for seg in [[tl, tr], [tr, br], [br, bl], [bl, tl]] {
            painter.line_segment(seg, border);
        }

        // value ticks + units — both in the table's DECLARED unit space
        // (an mph velocity table ticks in mph under an mph chip).
        let (unit_label, unit_scale) = table_display_unit(table, product, self.units());
        let label_color = egui::Color32::from_rgb(214, 220, 228);
        let font = egui::FontId::proportional(11.0);
        let decimals = if (vmax - vmin) / unit_scale < 5.0 {
            2
        } else {
            0
        };
        for t in 0..=5 {
            let f = t as f32 / 5.0;
            let v = (vmax + (vmin - vmax) * f) / unit_scale;
            let y = top + bar_h * f;
            painter.text(
                egui::pos2(x0 - 5.0, y),
                egui::Align2::RIGHT_CENTER,
                format!("{v:.*}", decimals),
                font.clone(),
                label_color,
            );
        }
        painter.text(
            egui::pos2(x0 + bar_w * 0.5, top - 10.0),
            egui::Align2::CENTER_CENTER,
            unit_label,
            font,
            label_color,
        );
    }

    /// Floating inspector card: a compact data card at the hover position (or
    /// at the Shift+click-pinned geo point, which tracks pan/zoom and updates
    /// with each new volume). Velocity products also get a radial arrow at
    /// the probed gate: pointing along the beam, away from the radar for
    /// outbound, toward it for inbound, colored from the active table.
    pub(crate) fn draw_cursor_inspector(
        &mut self,
        painter: &egui::Painter,
        rect: egui::Rect,
        hover: Option<egui::Pos2>,
    ) {
        if !self.show_inspector_card {
            return;
        }
        // The card works with OR without radar data under the cursor —
        // a clear-air site (KDDC with no echoes) still reads coordinates
        // and the model value.
        let (anchor, readout, pinned) = if let Some((lon, lat)) = self.pinned_inspector_lonlat {
            let position = self.lon_lat_to_screen(rect, lon, lat);
            if !rect.contains(position) {
                return;
            }
            (position, self.cursor_readout_at(rect, position), true)
        } else if let Some(position) = hover {
            (position, self.cursor_readout.clone(), false)
        } else {
            return;
        };

        // Velocity radial arrow at the probed gate.
        if let Some(readout) = &readout
            && readout.product.is_signed_radial_velocity()
            && readout.value.is_finite()
            && let Some((radar_lat, radar_lon)) = self.radar_location()
        {
            let radar_pos = self.lon_lat_to_screen(rect, radar_lon, radar_lat);
            let away = anchor - radar_pos;
            if away.length() > 1.0 {
                let direction = away.normalized() * if readout.value >= 0.0 { 1.0 } else { -1.0 };
                let color = {
                    let table = self.color_tables.for_family(ColorTableFamily::Velocity);
                    let c = table.color_for_value(readout.value);
                    egui::Color32::from_rgb(c[0], c[1], c[2])
                };
                let vector = direction * 26.0;
                painter.arrow(anchor, vector, egui::Stroke::new(4.0, egui::Color32::BLACK));
                painter.arrow(anchor, vector, egui::Stroke::new(2.0, color));
            }
        }
        if pinned {
            painter.circle_filled(anchor, 3.0, egui::Color32::from_rgb(255, 226, 120));
            painter.circle_stroke(anchor, 5.0, egui::Stroke::new(1.2, egui::Color32::BLACK));
        }
        let history_frame_time = self.surface_obs_frame_time_utc();
        let obs_history = self
            .pinned_obs_chart_station
            .as_deref()
            .and_then(|station| self.obs_history_data(station, history_frame_time));

        // Card lines. The coordinate line is ALWAYS shown (Solarpower07's
        // loupe convention: N/S + E/W); `swatches` pins a color chip to a
        // line index so the field's color rides next to its #RRGGBB value.
        let mut lines = Vec::new();
        let mut swatches: Vec<(usize, egui::Color32)> = Vec::new();
        let (cursor_lon, cursor_lat) = self.screen_to_lon_lat(rect, anchor);
        lines.push(format_lat_lon(cursor_lat, cursor_lon));
        if let Some(readout) = &readout {
            // Value in the active table's declared unit space (an mph
            // velocity table reads out mph) — the SI diagnostics below
            // (raw VEL, Nyquist) deliberately stay m/s.
            let (units, unit_scale) = table_display_unit(
                self.active_table_for_product(&readout.product),
                &readout.product,
                self.units(),
            );
            lines.push(format!(
                "{} {:.1}{}{}",
                readout.product.label(),
                readout.value / unit_scale,
                if units.is_empty() { "" } else { " " },
                units
            ));
            // Color chip + hex from the SAME palette the raster is painted
            // with (value → table color, never a framebuffer read). A
            // transparent sample (below display threshold) contributes no
            // chip — the gate reads out but paints nothing.
            if self.inspector_show_hex {
                let c = self
                    .active_table_for_product(&readout.product)
                    .color_for_value(readout.value);
                if c[3] > 0 {
                    swatches.push((lines.len(), egui::Color32::from_rgb(c[0], c[1], c[2])));
                    lines.push(hex_rgb_line(c));
                }
            }
            if !self.inspector_show_raw_vel {
            } else if let Some(base) = readout.base_value {
                let nyquist = readout
                    .nyquist_velocity_mps
                    .map(|n| format!(" · Nyq {n:.0}"))
                    .unwrap_or_default();
                lines.push(format!("raw VEL {base:.1} m/s{nyquist}"));
            } else if readout.product.is_signed_radial_velocity()
                && let Some(nyquist) = readout.nyquist_velocity_mps
                && readout.value.abs() >= nyquist * 0.75
            {
                // RAW velocity near the Nyquist can be folded — a folded gate
                // reads as opposite-direction flow (a fake couplet). Same field
                // failure that motivated this: blue at +23.5 m/s that was really
                // −33 with Nyq 28.
                lines.push(format!(
                    "⚠ near Nyquist ({nyquist:.0}) — may be folded; enable Unfold VEL"
                ));
            }
            if self.inspector_show_range_az {
                lines.push(format!(
                    "{:.1} km @ {:03.0}° · tilt {:.1}°",
                    readout.range_km, readout.azimuth_deg, readout.elevation_deg
                ));
            }
            if self.inspector_show_beam {
                let beam_m = readout.height_above_radar_m;
                lines.push(match self.units() {
                    // Imperial keeps the m + kft dual (analyst convention).
                    units::Units::Imperial => format!(
                        "beam ↑ {beam_m:.0} m ({})",
                        units::format_beam_height(beam_m, units::Units::Imperial)
                    ),
                    units::Units::Metric => format!(
                        "beam ↑ {}",
                        units::format_beam_height(beam_m, units::Units::Metric)
                    ),
                });
            }
        }
        // Nearest surface ob under the cursor (within ~28 px) — the full
        // decoded report, with age.
        let ob_owns_card = if self.obs_enabled && !self.surface_obs.is_empty() {
            let frame_time = self.surface_obs_frame_time_utc();
            let mut best: Option<(f32, &obs::SurfaceOb)> = None;
            for ob in self.surface_obs.frame_obs(frame_time) {
                let pos = self.lon_lat_to_screen(rect, ob.lon, ob.lat);
                let d = pos.distance(anchor);
                if d < 40.0 && best.map(|(bd, _)| d < bd).unwrap_or(true) {
                    best = Some((d, ob));
                }
            }
            let ob_matched = best.is_some();
            if let Some((_, ob)) = best {
                let mut line = ob.station_id.clone();
                if let (Some(t), Some(td)) = (ob.temp_c, ob.dewpoint_c) {
                    line.push_str(&format!(
                        " {}",
                        units::format_temp_pair_c(t, td, self.units())
                    ));
                } else if let Some(t) = ob.temp_c {
                    // T-only stations (some mesonet sensors skip dewpoint)
                    // still get their temperature on the card.
                    line.push_str(&format!(
                        " {}",
                        units::format_temperature_c(t, self.units())
                    ));
                }
                if let (Some(dir), Some(spd)) = (ob.wind_dir_deg, ob.wind_speed_kt) {
                    line.push_str(&format!(" {dir:03.0}°/{spd:.0}"));
                    if let Some(gust) = ob.wind_gust_kt {
                        line.push_str(&format!("G{gust:.0}"));
                    }
                    line.push_str("kt");
                }
                if let Some(altim) = ob.altim_in_hg {
                    line.push_str(&format!(" {altim:.2}\""));
                }
                if let Some(time) = ob.time_utc {
                    let age_min = (frame_time - time).num_minutes();
                    line.push_str(&format!(" · {age_min}m"));
                }
                // The ob owns the card near a station: float it to the
                // top of the data lines (field critique: the model line
                // was smothering it).
                let at = lines.len().min(1);
                lines.insert(at, line);
                // Keep color chips anchored to their (now shifted) lines.
                for swatch in &mut swatches {
                    if swatch.0 >= at {
                        swatch.0 += 1;
                    }
                }
            }
            ob_matched
        } else {
            false
        };
        // Model values under the cursor: read EVERY visible layer,
        // topmost first to match the visual stack (each slot carries its
        // own field + LUT), so a base field and its OA/derived siblings
        // read out together (field request). Falls back to the dock's
        // field when no layer produced a value.
        let _ = ob_owns_card;
        if self.model_enabled && self.inspector_show_model {
            let mut shown = false;
            // The topmost model layer contributes the color chip, but only
            // when no radar gate already owns the value line (then the chip
            // above matches the radar raster, not the model raster).
            let mut model_chip: Option<(usize, egui::Color32)> = None;
            for slot in self
                .model_layers
                .iter()
                .rev()
                .filter(|slot| slot.layer.visible)
            {
                let field = &slot.layer.field;
                if let Some(index) = slot.layer.lut.lookup(cursor_lat, cursor_lon)
                    && let Some(value) = field.values.get(index).copied()
                    && value.is_finite()
                {
                    lines.push(format!("{} {:.1} {}", field.key.var, value, field.units));
                    if model_chip.is_none()
                        && readout.is_none()
                        && self.inspector_show_hex
                        && let Some(c) = self.model_layer_color_for_value(&slot.layer, value)
                        && c[3] > 0
                    {
                        model_chip = Some((lines.len(), egui::Color32::from_rgb(c[0], c[1], c[2])));
                        lines.push(hex_rgb_line(c));
                    }
                    shown = true;
                }
            }
            if let Some(chip) = model_chip {
                swatches.push(chip);
            }
            if !shown
                && let Some((_, lut)) = &self.model_lut
                && let Some(field) = self
                    .model_dock
                    .as_ref()
                    .and_then(|dock| dock.latest_field())
                && let Some(index) = lut.lookup(cursor_lat, cursor_lon)
                && let Some(value) = field.values.get(index).copied()
                && value.is_finite()
            {
                lines.push(format!(
                    "{} {} {:.1} {}",
                    field.key.hour.model.to_uppercase(),
                    field.key.var,
                    value,
                    field.units
                ));
            }
        }
        if let Some(probe) = readout.as_ref().and_then(|readout| readout.vrot) {
            lines.push(format_vrot_card_line(
                probe,
                self.vrot_display_unit(),
                self.units(),
            ));
        }
        if let Some(history) = &obs_history {
            let network = if history.network.is_empty() {
                "obs"
            } else {
                history.network.as_str()
            };
            lines.push(format!(
                "{} {} 3h obs - {} reports",
                history.station_id,
                network,
                history.rows.len()
            ));
        } else if let Some(station) = &self.pinned_obs_chart_station {
            lines.push(format!("{station} 3h obs - no reports"));
        }
        // Discoverable path into the 3h obs timeline: hovering a drawn
        // station advertises the Shift+click pin (the context-menu row is
        // unreachable when "right-click loads closest radar" is on).
        if !pinned
            && self.pinned_obs_chart_station.is_none()
            && let Some(ob) = hover.and_then(|position| {
                self.surface_ob_near_screen(rect, position, OBS_TIMELINE_PIN_CLICK_PX)
            })
        {
            lines.push(format!(
                "{} {} — Shift+click: pin 3h timeline",
                ob.station_id, ob.network
            ));
        }
        if pinned {
            lines.push("pinned — Shift+click to release".to_owned());
        }

        let font = egui::FontId::monospace(11.0);
        let text_color = egui::Color32::from_rgb(222, 228, 236);
        let galleys: Vec<_> = lines
            .iter()
            .map(|line| painter.layout_no_wrap(line.clone(), font.clone(), text_color))
            .collect();
        // Graphical timeline strip (temp/dewpoint/wind/pressure sparklines
        // plus a frame-time scrubber marker) rides above the text table.
        let timeline_height = if obs_history.is_some() { 92.0 } else { 0.0 };
        let max_history_rows =
            ((rect.height() - 130.0 - timeline_height) / 13.5).clamp(4.0, 28.0) as usize;
        let history_rows_shown = obs_history
            .as_ref()
            .map(|history| history.rows.len().min(max_history_rows))
            .unwrap_or(0);
        let history_has_more = obs_history
            .as_ref()
            .map(|history| history.rows.len() > history_rows_shown)
            .unwrap_or(false);
        let history_height = if history_rows_shown > 0 {
            30.0 + history_rows_shown as f32 * 13.5 + if history_has_more { 13.5 } else { 0.0 }
        } else {
            0.0
        };
        let history_width = obs_history.as_ref().map(|_| 444.0).unwrap_or(0.0);
        let width = (galleys.iter().map(|g| g.size().x).fold(0.0f32, f32::max) + 14.0)
            .max(history_width + 14.0);
        let history_extra = if obs_history.is_some() {
            history_height + timeline_height + 12.0
        } else {
            0.0
        };
        let height = galleys.iter().map(|g| g.size().y + 2.0).sum::<f32>() + 10.0 + history_extra;
        let mut origin = anchor + egui::vec2(16.0, 14.0);
        if origin.x + width > rect.right() - 4.0 {
            origin.x = anchor.x - 16.0 - width;
        }
        if origin.y + height > rect.bottom() - 4.0 {
            origin.y = anchor.y - 14.0 - height;
        }
        let card = egui::Rect::from_min_size(origin, egui::vec2(width, height));
        painter.rect_filled(
            card,
            5.0,
            egui::Color32::from_rgba_unmultiplied(12, 15, 20, 232),
        );
        let border = egui::Stroke::new(
            1.0,
            if pinned {
                egui::Color32::from_rgb(255, 226, 120)
            } else {
                egui::Color32::from_rgb(60, 70, 84)
            },
        );
        painter.line_segment([card.left_top(), card.right_top()], border);
        painter.line_segment([card.right_top(), card.right_bottom()], border);
        painter.line_segment([card.right_bottom(), card.left_bottom()], border);
        painter.line_segment([card.left_bottom(), card.left_top()], border);
        let mut y = card.top() + 5.0;
        for (idx, galley) in galleys.into_iter().enumerate() {
            let size = galley.size();
            painter.galley(egui::pos2(card.left() + 7.0, y), galley, text_color);
            // Color chip in the reserved left gutter of a #RRGGBB line.
            if let Some((_, color)) = swatches.iter().find(|(line, _)| *line == idx) {
                let chip = 11.0;
                let chip_rect = egui::Rect::from_min_size(
                    egui::pos2(card.left() + 8.0, y + (size.y - chip) * 0.5),
                    egui::vec2(chip, chip),
                );
                painter.rect_filled(chip_rect, 2.0, *color);
                painter.rect_stroke(
                    chip_rect,
                    2.0,
                    egui::Stroke::new(1.0, egui::Color32::from_gray(20)),
                    egui::StrokeKind::Outside,
                );
            }
            y += size.y + 2.0;
        }
        if let Some(history) = &obs_history {
            let mut strip_top = y + 3.0;
            if timeline_height > 0.0 {
                let timeline_rect = egui::Rect::from_min_size(
                    egui::pos2(card.left() + 7.0, strip_top),
                    egui::vec2(width - 14.0, timeline_height),
                );
                draw_obs_history_timeline(painter, timeline_rect, history, self.units());
                strip_top = timeline_rect.bottom() + 4.0;
            }
            if history_height > 0.0 {
                let history_rect = egui::Rect::from_min_size(
                    egui::pos2(card.left() + 7.0, strip_top),
                    egui::vec2(width - 14.0, history_height),
                );
                draw_obs_history_table(
                    painter,
                    history_rect,
                    history,
                    history_rows_shown,
                    self.units(),
                );
            }
        }
        // Field Loupe: circular GPU magnifier at the focus point. Drawn last
        // so it rides above the card when the two overlap.
        self.draw_cursor_loupe(painter, rect, anchor, readout.is_some());
    }

    /// A circular, pixelated GPU magnifier ("Field Loupe") anchored at the
    /// cursor — inspired by Solarpower07's WRF-Runner loupe.
    ///
    /// Fully native and GPU-resident: it builds ONE [`egui::Mesh`] triangle-fan
    /// over a disk and gives each vertex a texture UV computed by inverting the
    /// exact same raster transform the field is drawn with — the planar-ENU
    /// radar raster (rotate about the radar pivot + [`anchored_radar_texture_rect`])
    /// or the full-viewport model raster. The GPU then samples the field texture
    /// that is already uploaded, so there is NO CPU per-pixel loop and NO
    /// framebuffer / `getImageData`-style readback. The NEAREST sampler
    /// ([`radar_texture_options`]) supplies the pixelated look for free.
    ///
    /// Shown when the `inspector_show_loupe` toggle is on OR Shift is held.
    fn draw_cursor_loupe(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        anchor: egui::Pos2,
        radar_has_gate: bool,
    ) {
        let shift_held = painter.ctx().input(|input| input.modifiers.shift);
        if !(self.inspector_show_loupe || shift_held) {
            return;
        }
        // v0.29.3: while the plot-domain box tool is engaged the drag must
        // not summon the magnifier mid-box (owner constraint) — Ctrl+SHIFT
        // holds Shift down, and the armed mode owns the surface outright.
        // Loupe behavior everywhere else is untouched.
        if self.plot_domain_map_drag.is_some() || self.plot_domain_arm_active() {
            return;
        }
        if !rect.contains(anchor) {
            return;
        }
        let Some(source) = self.loupe_source(painter.ctx(), rect, anchor, radar_has_gate) else {
            return;
        };

        // Disk geometry. MAGNIFY is the loupe's optical zoom: a rim vertex at
        // screen offset `d` from the loupe center samples the field at
        // `focus + d / MAGNIFY`, so a small patch around the focus fills the
        // whole disk (the "scaled toward the cursor by 1/zoom" mapping).
        const RADIUS: f32 = 70.0;
        const MAGNIFY: f32 = 6.0;
        const RIM: usize = 72;
        let focus = anchor;

        // Edge-clamp: float the disk above the cursor, flip below if it would
        // clip the top, then keep the whole disk inside the pane.
        let margin = RADIUS + 6.0;
        let mut center = egui::pos2(anchor.x, anchor.y - (RADIUS + 34.0));
        if center.y - RADIUS < rect.top() + 4.0 {
            center.y = anchor.y + (RADIUS + 34.0);
        }
        center.x = center.x.clamp(rect.left() + margin, rect.right() - margin);
        center.y = center.y.clamp(rect.top() + margin, rect.bottom() - margin);

        let to_uv = |q: egui::Pos2| -> egui::Pos2 {
            source.screen_to_uv(loupe_sample_screen(focus, center, q, MAGNIFY))
        };
        let tint = egui::Color32::WHITE;
        let mut mesh = egui::epaint::Mesh::with_texture(source.texture_id);
        mesh.vertices.push(egui::epaint::Vertex {
            pos: center,
            uv: to_uv(center),
            color: tint,
        });
        for i in 0..RIM {
            let theta = std::f32::consts::TAU * i as f32 / RIM as f32;
            let q = center + RADIUS * egui::vec2(theta.cos(), theta.sin());
            mesh.vertices.push(egui::epaint::Vertex {
                pos: q,
                uv: to_uv(q),
                color: tint,
            });
        }
        for i in 0..RIM {
            let a = 1 + i as u32;
            let b = 1 + ((i + 1) % RIM) as u32;
            mesh.indices.extend_from_slice(&[0, a, b]);
        }
        painter.add(egui::Shape::mesh(mesh));

        // Ring (dark halo + bright rim) and a center crosshair marking the
        // exact focus gate/point.
        painter.circle_stroke(
            center,
            RADIUS,
            egui::Stroke::new(3.0, egui::Color32::from_rgb(12, 15, 20)),
        );
        painter.circle_stroke(
            center,
            RADIUS,
            egui::Stroke::new(1.4, egui::Color32::from_rgb(232, 238, 246)),
        );
        let cross = 8.0;
        let cross_stroke = egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 190),
        );
        painter.line_segment(
            [
                center - egui::vec2(cross, 0.0),
                center + egui::vec2(cross, 0.0),
            ],
            cross_stroke,
        );
        painter.line_segment(
            [
                center - egui::vec2(0.0, cross),
                center + egui::vec2(0.0, cross),
            ],
            cross_stroke,
        );
    }

    /// The field texture the loupe magnifies under the cursor, with the exact
    /// screen→UV inverse to sample it. Radar wins when a gate resolves under
    /// the cursor (it draws on top); otherwise the topmost visible model layer
    /// the value line is reading takes over, falling back to the radar raster.
    fn loupe_source(
        &self,
        ctx: &egui::Context,
        rect: egui::Rect,
        anchor: egui::Pos2,
        radar_has_gate: bool,
    ) -> Option<LoupeSource> {
        let (lon, lat) = self.screen_to_lon_lat(rect, anchor);
        let radar = self.loupe_radar_source(ctx, rect);
        let model = self.loupe_model_source(rect, lat, lon);
        match (radar_has_gate, radar, model) {
            (true, Some(radar), _) => Some(radar),
            (_, _, Some(model)) => Some(model),
            (_, Some(radar), None) => Some(radar),
            _ => None,
        }
    }

    /// The primary radar raster + its screen→UV inverse (mirrors
    /// [`Self::draw_radar_layer`] so the loupe samples exactly what is drawn).
    fn loupe_radar_source(&self, ctx: &egui::Context, rect: egui::Rect) -> Option<LoupeSource> {
        let texture = self.texture.as_ref()?;
        let key = self.texture_key.as_ref()?;
        let (radar_lat, radar_lon) = self.radar_location()?;
        let image_rect = self.radar_texture_rect(ctx, rect, radar_lat, radar_lon, key);
        let baked = pane_or_key_rotation_rad(&self.texture_key);
        let angle = self.aeqd_north_angle(rect, radar_lat, radar_lon) - baked;
        let pivot = self.lon_lat_to_screen(rect, radar_lon, radar_lat);
        Some(LoupeSource {
            texture_id: texture.id(),
            kind: LoupeKind::Rotated {
                image_rect,
                pivot,
                angle,
            },
        })
    }

    /// The topmost visible model layer whose rendered texture matches the
    /// current view AND resolves a finite value at the cursor. That texture
    /// is painted 1:1 across the pane, so its screen→UV inverse is a plain
    /// normalization within `rect`.
    fn loupe_model_source(&self, rect: egui::Rect, lat: f32, lon: f32) -> Option<LoupeSource> {
        if !self.model_enabled {
            return None;
        }
        let view = self.model_layer_current_view();
        for slot in self.model_layers.iter().rev().filter(|s| s.layer.visible) {
            let Some((texture, generation, rendered)) = slot.texture.as_ref() else {
                continue;
            };
            if *generation != slot.layer.generation
                || model_layer_view_needs_rerender(rendered, &view)
            {
                continue;
            }
            let Some(index) = slot.layer.lut.lookup(lat, lon) else {
                continue;
            };
            let Some(value) =
                model_layer::sample_field_value(slot.layer.field.as_ref(), index, lat, lon)
            else {
                continue;
            };
            if !value.is_finite() {
                continue;
            }
            return Some(LoupeSource {
                texture_id: texture.id(),
                kind: LoupeKind::Rect { rect },
            });
        }
        None
    }

    /// The on-screen color a model layer paints for `value`. Resolves through
    /// [`model_layer_value_color`] — the SAME function
    /// [`Self::draw_model_layers`]' raster loop paints with (BowEcho palette
    /// override → Solar model table → production style → generic ramp) — so
    /// the chip cannot drift from the painted pixel. (Historically this
    /// re-implemented the precedence and skipped `model_table`, so for WRF
    /// layers the chip showed the production color while the map painted
    /// Solar.)
    pub(crate) fn model_layer_color_for_value(
        &self,
        layer: &model_layer::ModelMapLayer,
        value: f32,
    ) -> Option<[u8; 4]> {
        if !value.is_finite() {
            return None;
        }
        let custom_table = layer
            .custom_color_family
            .map(|family| self.color_tables.for_family(family));
        model_layer_value_color(
            custom_table,
            layer.model_table.as_deref(),
            layer.production.as_deref(),
            layer.field.range,
            layer.colormap,
            value,
        )
        .map(|color| color.to_array())
    }

    /// Shift+click: pin the inspector to a geo point (or release a pin when
    /// clicking within a few pixels of it). Shift+click on a DRAWN surface-ob
    /// station pins that station's 3h obs timeline instead of a plain point —
    /// the discoverable sibling of the context-menu "Pin ... 3h timeline"
    /// row, which the "right-click loads closest radar" preference otherwise
    /// makes unreachable. (Historically this gesture CLEARED the timeline
    /// even when the click landed on the station itself.)
    pub(crate) fn toggle_inspector_pin(&mut self, rect: egui::Rect, pointer: egui::Pos2) {
        if let Some((lon, lat)) = self.pinned_inspector_lonlat {
            let current = self.lon_lat_to_screen(rect, lon, lat);
            if current.distance(pointer) <= 14.0 {
                self.pinned_inspector_lonlat = None;
                self.pinned_obs_chart_station = None;
                return;
            }
        }
        if let Some(ob) = self.surface_ob_near_screen(rect, pointer, OBS_TIMELINE_PIN_CLICK_PX) {
            self.pin_obs_history(&ob);
            return;
        }
        let (lon, lat) = self.screen_to_lon_lat(rect, pointer);
        self.pinned_inspector_lonlat = Some((lon, lat));
        self.pinned_obs_chart_station = None;
    }

    pub(crate) fn model_soundings_available(&self) -> bool {
        self.model_enabled && self.model_lut.is_some()
    }

    pub(crate) fn request_model_sounding_at_lonlat(&mut self, lon: f32, lat: f32) -> bool {
        // The LUT rebuild is async: pair the LUT with the field only when
        // their grid hashes agree, or a stale grid's LUT decomposes indices
        // on the new field's nx.
        let lookup = self.model_lut.as_ref().and_then(|(hash, lut)| {
            let field = self
                .model_dock
                .as_ref()
                .and_then(|dock| dock.latest_field())?;
            if field.grid.as_ref().map(|grid| &grid.hash) != Some(hash) {
                return None;
            }
            lut.lookup(lat, lon)
                .map(|index| (index, field.nx, field.key.hour.model.clone()))
        });
        let Some((index, nx, lut_model)) = lookup else {
            return false;
        };
        if self.last_sounding_request == Some(index) {
            return false;
        }
        let target = self.displayed_timeline_time_utc().unwrap_or_else(Utc::now);
        let Some(dock) = &mut self.model_dock else {
            return false;
        };
        let fx = (index % nx) as f64;
        let fy = (index / nx) as f64;
        // Mixed hrrr+gfs stores: these grid coords belong to the LUT's
        // model. The browser-selected hour is only safe to sample when the
        // models agree; otherwise pin to the LUT model's hour valid nearest
        // the display time.
        if dock.browsed_hour_model().as_deref() == Some(lut_model.as_str()) {
            dock.request_sounding_at(fx, fy);
            self.last_sounding_request = Some(index);
            true
        } else if let Some((key, _, _)) = dock.newest_hour_valid_near(target, Some(&lut_model)) {
            dock.request_sounding_for(key, fx, fy);
            self.last_sounding_request = Some(index);
            true
        } else {
            false
        }
    }

    pub(crate) fn draw_world_place_labels(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let Some(max_rank) = world_place_label_rank(self.map_scale) else {
            return;
        };
        self.draw_place_label_set(
            painter,
            rect,
            bounds,
            PlaceLabelSet {
                labels: basemap_data::BASEMAP_WORLD_PLACE_LABELS,
                max_rank,
                max_labels: world_label_budget(self.map_scale),
            },
            occupied,
        );
    }

    pub(crate) fn draw_regional_place_labels(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let Some(max_rank) = place_label_rank(self.map_scale) else {
            return;
        };
        let max_labels = label_budget(self.map_scale);
        if us_detail_visible(bounds) {
            self.draw_place_label_set(
                painter,
                rect,
                bounds,
                PlaceLabelSet {
                    labels: basemap_data::BASEMAP_US_PLACE_LABELS,
                    max_rank,
                    max_labels,
                },
                occupied,
            );
            // Dense Census small-town layer (32k places) at storm zoom — the
            // towns a warning forecaster calls out on stream. Drawn AFTER the
            // city set so the occupied list keeps city names dominant.
            if let Some(town_rank) = town_label_rank(self.map_scale) {
                self.draw_place_label_set(
                    painter,
                    rect,
                    bounds,
                    PlaceLabelSet {
                        labels: basemap_towns::BASEMAP_US_TOWN_LABELS,
                        max_rank: town_rank,
                        max_labels: 70,
                    },
                    occupied,
                );
            }
        }
        for layer in REGIONAL_BASEMAP_LAYERS {
            if bounds.intersects_bbox(layer.bounds) {
                self.draw_place_label_set(
                    painter,
                    rect,
                    bounds,
                    PlaceLabelSet {
                        labels: layer.place_labels,
                        max_rank,
                        max_labels,
                    },
                    occupied,
                );
            }
        }
    }

    fn draw_place_label_set(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        place_labels: PlaceLabelSet,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let bold = self.bold_labels;
        // GR2-style callouts: bold white with a heavy dark outline so a
        // meteorologist can read town names over a red core on stream.
        let (base_text_color, base_halo_color, base_dot_color) = if bold {
            (
                egui::Color32::WHITE,
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 235),
                egui::Color32::from_rgb(235, 238, 242),
            )
        } else {
            (
                egui::Color32::WHITE,
                egui::Color32::from_rgba_unmultiplied(3, 5, 8, 230),
                egui::Color32::from_rgb(210, 222, 232),
            )
        };
        let zoomed = self.map_scale >= 190.0;
        let mut drawn = 0usize;

        for label in place_labels.labels {
            if label.rank > place_labels.max_rank {
                break;
            }
            if !bounds.contains(label.lon, label.lat) {
                continue;
            }
            let (text_color, halo_color, dot_color) = if !bold && label.rank >= 7 {
                (
                    egui::Color32::WHITE,
                    egui::Color32::from_rgba_unmultiplied(3, 5, 8, 235),
                    egui::Color32::from_rgb(205, 218, 230),
                )
            } else {
                (base_text_color, base_halo_color, base_dot_color)
            };
            // Size tiers: bigger towns get bigger type (callout hierarchy);
            // the style registry's town scale multiplies every tier.
            let size = if bold {
                match label.rank {
                    0..=3 => 18.0,
                    4..=6 => 16.0,
                    _ => 15.0,
                }
            } else if zoomed {
                12.0
            } else {
                11.0
            } * self.style_registry.labels().town_font_scale;
            let font = egui::FontId::proportional(size);
            let position = self.lon_lat_to_screen(rect, label.lon, label.lat);
            if !rect.expand(32.0).contains(position) {
                continue;
            }
            let text_position = egui::pos2(position.x + 4.0, position.y - 1.0);
            let label_rect = left_label_rect(text_position, label.name, font.size).expand(2.0);
            if !rect.expand(80.0).intersects(label_rect) || overlaps_any(occupied, label_rect) {
                continue;
            }
            painter.circle_filled(position, if bold { 2.2 } else { 1.5 }, dot_color);
            if bold {
                draw_heavy_halo_text(
                    painter,
                    text_position,
                    egui::Align2::LEFT_CENTER,
                    label.name,
                    font,
                    text_color,
                    halo_color,
                );
            } else {
                draw_halo_text(
                    painter,
                    text_position,
                    egui::Align2::LEFT_CENTER,
                    label.name,
                    font,
                    text_color,
                    halo_color,
                );
            }
            occupied.push(label_rect);
            drawn += 1;
            if drawn >= place_labels.max_labels {
                break;
            }
        }
    }

    pub(crate) fn draw_admin_labels(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        occupied: &mut Vec<egui::Rect>,
    ) {
        if self.app_settings.basemap_lightweight {
            return;
        }
        if self.map_scale < 55.0 {
            return;
        }
        let max_labels = if self.map_scale >= 220.0 { 72 } else { 36 };
        if us_detail_visible(bounds) {
            if self.map_scale <= 360.0 {
                self.draw_us_state_label_set(painter, rect, bounds, occupied);
            }
            if self.map_scale >= 118.0 {
                self.draw_admin_label_set(
                    painter,
                    rect,
                    bounds,
                    basemap_data::BASEMAP_US_COUNTY_LABELS,
                    max_labels,
                    occupied,
                );
            }
        }
        for layer in REGIONAL_BASEMAP_LAYERS {
            if bounds.intersects_bbox(layer.bounds) {
                self.draw_admin_label_set(
                    painter,
                    rect,
                    bounds,
                    layer.admin_labels,
                    max_labels,
                    occupied,
                );
            }
        }
    }

    fn draw_us_state_label_set(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let font = egui::FontId::proportional(13.0);
        let text_color = egui::Color32::WHITE;
        let halo_color = egui::Color32::from_rgba_unmultiplied(2, 4, 7, 235);
        for label in US_STATE_ANCHORS {
            let Some(name) = us_state_name(label.abbr) else {
                continue;
            };
            if !bounds.contains(label.lon, label.lat) {
                continue;
            }
            let position = self.lon_lat_to_screen(rect, label.lon, label.lat);
            if !rect.expand(32.0).contains(position) {
                continue;
            }
            let label_rect = centered_label_rect(position, name, font.size).expand(6.0);
            if !rect.expand(80.0).intersects(label_rect) || overlaps_any(occupied, label_rect) {
                continue;
            }
            draw_halo_text(
                painter,
                position,
                egui::Align2::CENTER_CENTER,
                name,
                font.clone(),
                text_color,
                halo_color,
            );
            occupied.push(label_rect);
        }
    }

    fn draw_admin_label_set(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        labels: &[basemap_data::BasemapLabel],
        max_labels: usize,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let font = egui::FontId::proportional(10.0);
        let text_color = egui::Color32::from_rgba_unmultiplied(150, 164, 176, 184);
        let halo_color = egui::Color32::from_rgba_unmultiplied(2, 4, 7, 180);
        let mut drawn = 0usize;

        for label in labels {
            if !bounds.contains(label.lon, label.lat) {
                continue;
            }
            let position = self.lon_lat_to_screen(rect, label.lon, label.lat);
            if !rect.expand(24.0).contains(position) {
                continue;
            }
            let label_rect = centered_label_rect(position, label.name, font.size).expand(5.0);
            if !rect.expand(80.0).intersects(label_rect) || overlaps_any(occupied, label_rect) {
                continue;
            }
            draw_halo_text(
                painter,
                position,
                egui::Align2::CENTER_CENTER,
                label.name,
                font.clone(),
                text_color,
                halo_color,
            );
            occupied.push(label_rect);
            drawn += 1;
            if drawn >= max_labels {
                break;
            }
        }
    }

    pub(crate) fn draw_graticule(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !self.app_settings.show_lat_lon_grid {
            return;
        }
        let bounds = self.visible_geo_bounds(rect);
        let lon_min = bounds.west;
        let lon_max = bounds.east;
        let lat_min = bounds.south;
        let lat_max = bounds.north;
        let step = graticule_step(rect.width() / self.lon_pixels_per_degree());
        let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(28, 38, 50));
        let label_color = egui::Color32::from_rgb(92, 108, 124);

        // Meridians and parallels are ARCS under AEQD — sample as polylines
        // (review finding F4).
        const GRATICULE_SEGMENTS: usize = 32;
        let mut lon = (lon_min / step).floor() * step;
        while lon <= lon_max {
            let points: Vec<egui::Pos2> = (0..=GRATICULE_SEGMENTS)
                .map(|i| {
                    let lat = lat_min + (lat_max - lat_min) * i as f32 / GRATICULE_SEGMENTS as f32;
                    self.lon_lat_to_screen(rect, lon, lat)
                })
                .collect();
            let top = points[GRATICULE_SEGMENTS];
            painter.add(egui::Shape::line(points, stroke));
            painter.text(
                egui::pos2(top.x + 4.0, rect.top() + 6.0),
                egui::Align2::LEFT_TOP,
                format!("{:.0}", normalize_lon(lon)),
                egui::FontId::monospace(10.0),
                label_color,
            );
            lon += step;
        }

        let mut lat = (lat_min / step).floor() * step;
        while lat <= lat_max {
            let points: Vec<egui::Pos2> = (0..=GRATICULE_SEGMENTS)
                .map(|i| {
                    let lon = lon_min + (lon_max - lon_min) * i as f32 / GRATICULE_SEGMENTS as f32;
                    self.lon_lat_to_screen(rect, lon, lat)
                })
                .collect();
            let left = points[0];
            painter.add(egui::Shape::line(points, stroke));
            painter.text(
                egui::pos2(rect.left() + 6.0, left.y - 2.0),
                egui::Align2::LEFT_CENTER,
                format!("{lat:.0}"),
                egui::FontId::monospace(10.0),
                label_color,
            );
            lat += step;
        }
    }

    pub(crate) fn draw_range_ring(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        latitude_deg: f32,
        longitude_deg: f32,
        range_km: f32,
        stroke: egui::Stroke,
    ) {
        let (lat_radius, lon_radius) = range_radius_deg(latitude_deg, range_km);
        let mut points = Vec::with_capacity(97);
        for index in 0..=96 {
            let angle = index as f32 / 96.0 * std::f32::consts::TAU;
            let latitude = latitude_deg + lat_radius * angle.sin();
            let longitude = longitude_deg + lon_radius * angle.cos();
            points.push(self.lon_lat_to_screen(rect, longitude, latitude));
        }
        painter.add(egui::Shape::line(points, stroke));
    }

    /// Hover readout for derived products via a one-shot grid cache.
    fn derived_cursor_readout(
        &mut self,
        rect: egui::Rect,
        position: egui::Pos2,
        derived: DerivedProduct,
        volume: &Arc<RadarVolume>,
        selected_cut: usize,
    ) -> Option<CursorReadout> {
        let volume_ptr = Arc::as_ptr(volume) as usize;
        let smoothing = self.smoothing_for_product(&DisplayProduct::Derived(derived));
        let hail_key = self.hail_levels_key();
        let cut_key = if derived.is_volume_wide() {
            usize::MAX
        } else {
            selected_cut
        };
        let cached = self
            .derived_readout_cache
            .as_ref()
            .filter(|cache| {
                cache.product == derived
                    && cache.volume_ptr == volume_ptr
                    && cache.cut_key == cut_key
                    && cache.smoothing == smoothing
                    && cache.hail_key == hail_key
            })
            .map(|cache| (cache.base_idx, Arc::clone(&cache.grid)));
        let (base_idx, grid) = match cached {
            Some(hit) => hit,
            None => {
                let hail = self.hail_levels_m();
                let (base_idx, grid) = if derived.is_volume_wide() {
                    let base_moment = derived.base_moment();
                    let base_idx = volume
                        .cuts
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.moments.contains_key(&base_moment))
                        .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
                        .map(|(i, _)| i)?;
                    let grid = match derived {
                        DerivedProduct::CompositeReflectivity => {
                            composite_reflectivity_grid(volume)
                        }
                        DerivedProduct::EchoTops => echo_top_grid(volume, ECHO_TOP_THRESHOLD_DBZ),
                        DerivedProduct::Vil => vil_grid(volume),
                        DerivedProduct::VilDensity => vil_density_grid(volume),
                        DerivedProduct::Mehs => mehs_grid(volume, hail.0, hail.1),
                        DerivedProduct::Posh => {
                            hail_grids(volume, hail.0, hail.1, render2d::MeshCalibration::Witt1998)
                                .map(|grids| grids.posh_pct)
                        }
                        DerivedProduct::Poh => poh_grid(volume, hail.0),
                        DerivedProduct::Marc => marc_grid(volume),
                        DerivedProduct::GustProxy => gust_proxy_grid(volume),
                        DerivedProduct::AzimuthalShear | DerivedProduct::Divergence => None,
                    }?;
                    (base_idx, grid)
                } else {
                    let cut = volume.cuts.get(selected_cut)?;
                    let velocity = cut.moments.get(&MomentType::Velocity)?;
                    let grid = match derived {
                        DerivedProduct::AzimuthalShear => {
                            render2d::azimuthal_shear_grid(cut, velocity)
                        }
                        DerivedProduct::Divergence => {
                            render2d::radial_divergence_grid(cut, velocity)
                        }
                        _ => return None,
                    };
                    (selected_cut, grid)
                };
                let grid = match smoothing {
                    SmoothingMode::Native | SmoothingMode::Interpolated => grid,
                    SmoothingMode::Soften => smooth_moment_grid(&grid),
                };
                let grid = Arc::new(grid);
                self.derived_readout_cache = Some(DerivedReadoutCache {
                    product: derived,
                    volume_ptr,
                    cut_key,
                    smoothing,
                    hail_key,
                    base_idx,
                    grid: Arc::clone(&grid),
                });
                (base_idx, grid)
            }
        };
        let cut = volume.cuts.get(base_idx)?;
        let (mut row, mut gate, mut radial_index, mut azimuth_deg, mut range_km, mut slant_range_m) =
            self.sample_grid_geometry(rect, position, cut, &grid)?;
        let (finite_row, finite_gate, value) =
            nearest_finite_grid_sample(&grid, row, gate, DERIVED_READOUT_FALLBACK_RADIUS)?;
        if finite_row != row || finite_gate != gate {
            row = finite_row;
            gate = finite_gate;
            radial_index = grid.radial_indices.get(row).copied()?;
            let radial = cut.radials.get(radial_index)?;
            azimuth_deg = radial.azimuth_deg.rem_euclid(360.0);
            slant_range_m = grid.gate_range.first_gate_m as f64
                + gate as f64 * grid.gate_range.gate_spacing_m as f64;
            range_km = (slant_range_m / 1000.0) as f32;
        }
        let source_azimuth_deg = cut
            .radials
            .get(radial_index)
            .map(|radial| radial.azimuth_deg)
            .unwrap_or(azimuth_deg);
        let height_above_radar_m =
            radar_core::beam_height_above_radar_m(slant_range_m, cut.elevation_deg as f64) as f32;
        Some(CursorReadout {
            site_id: volume.site.id.clone(),
            volume_time_utc: volume.volume_time.with_timezone(&Utc),
            product: DisplayProduct::Derived(derived),
            cut: base_idx,
            value,
            base_value: None,
            vrot: None,
            raw: None,
            row,
            gate,
            gate_spacing_m: grid.gate_range.gate_spacing_m,
            range_km,
            azimuth_deg,
            source_azimuth_deg,
            elevation_deg: cut.elevation_deg,
            height_above_radar_m,
            nyquist_velocity_mps: None,
            realtime_volume_id: None,
            realtime_last_chunk_id: None,
            realtime_last_chunk_type: None,
        })
    }

    /// Invert the raster's screen mapping to (row, gate, azimuth, range,
    /// slant range) on a cut/grid — shared by the moment and derived
    /// readout paths.
    fn sample_grid_geometry(
        &self,
        rect: egui::Rect,
        position: egui::Pos2,
        cut: &ElevationCut,
        grid: &MomentGrid,
    ) -> Option<(usize, usize, usize, f32, f32, f64)> {
        let (radar_lat, radar_lon) = self.loaded_volume_location()?;
        let radar_pos = self.lon_lat_to_screen(rect, radar_lon, radar_lat);
        let angle = self.aeqd_north_angle(rect, radar_lat, radar_lon);
        let offset = position - radar_pos;
        let (sin, cos) = (-angle).sin_cos();
        let east_px = offset.x * cos - offset.y * sin;
        let north_px = -(offset.x * sin + offset.y * cos);
        let km_per_px = 111.32 / self.map_scale;
        let lon_km = east_px * km_per_px;
        let lat_km = north_px * km_per_px;
        let range_km = lat_km.hypot(lon_km);
        let max_range_km = grid_range_km(grid)?;
        if range_km > max_range_km {
            return None;
        }
        let mut azimuth_deg = lon_km.atan2(lat_km).to_degrees();
        if azimuth_deg < 0.0 {
            azimuth_deg += 360.0;
        }
        let (row, radial_index) = nearest_grid_row(cut, grid, azimuth_deg)?;
        let gate = gate_for_range(grid, range_km)?;
        let slant_range_m = grid.gate_range.first_gate_m as f64
            + gate as f64 * grid.gate_range.gate_spacing_m as f64;
        Some((
            row,
            gate,
            radial_index,
            azimuth_deg,
            range_km,
            slant_range_m,
        ))
    }

    pub(crate) fn cursor_readout_at(
        &mut self,
        rect: egui::Rect,
        position: egui::Pos2,
    ) -> Option<CursorReadout> {
        let product = self.selected_product.clone();
        let cut = self.selected_cut;
        self.cursor_readout_for(rect, position, &product, cut)
    }

    pub(crate) fn add_vrot_tool_point(
        &mut self,
        rect: egui::Rect,
        position: egui::Pos2,
        product: &DisplayProduct,
        cut: usize,
    ) {
        if !manual_vrot_product_supported(product) {
            self.status = "Vrot tool needs VEL, DVEL, SRV, or DSRV".to_owned();
            return;
        }
        let Some(readout) = self.cursor_readout_for(rect, position, product, cut) else {
            self.status = "No velocity gate under Vrot click".to_owned();
            return;
        };
        if !manual_vrot_product_supported(&readout.product) || !readout.value.is_finite() {
            self.status = "No velocity gate under Vrot click".to_owned();
            return;
        }
        let (lon, lat) = self.screen_to_lon_lat(rect, position);
        if self.vrot_points.len() >= 2 {
            self.vrot_points.clear();
        }
        self.vrot_points
            .push((lon, lat, readout.value, readout.height_above_radar_m));
        self.status = match self.vrot_points.as_slice() {
            [_] => "Vrot point 1 set; click the opposite velocity max".to_owned(),
            [a, b] => {
                let (velocity_unit, velocity_scale) = self.vrot_display_unit();
                let vrot = ((a.2.abs() + b.2.abs()) / 2.0) / velocity_scale;
                if a.2.signum() == b.2.signum() {
                    format!(
                        "Vrot {:.0} {velocity_unit}; points have the same sign",
                        vrot
                    )
                } else {
                    format!("Vrot {:.0} {velocity_unit} set", vrot)
                }
            }
            _ => String::new(),
        };
    }

    /// Readout for an arbitrary product/tilt — lets every grid pane report
    /// ITS OWN data under the cursor instead of the primary pane's.
    pub(crate) fn cursor_readout_for(
        &mut self,
        rect: egui::Rect,
        position: egui::Pos2,
        product: &DisplayProduct,
        cut_index: usize,
    ) -> Option<CursorReadout> {
        let volume = self
            .volume
            .clone()
            .map(|volume| self.display_source_volume_for_product(product, volume))?;
        let selected_cut = cut_index;
        let selected_product = product.clone();
        // Derived products sample a cached one-shot grid (computed on the
        // first hover, reused until the product or volume changes) — the
        // inspector works on EVERY product, not just raw moments.
        if let Some(derived) = selected_product.derived() {
            return self.derived_cursor_readout(rect, position, derived, &volume, selected_cut);
        }
        let cut = volume.cuts.get(selected_cut)?;
        let base_moment = selected_product.base_moment();
        let source_grid = cut.moments.get(&base_moment)?;
        let dealiased_grid = selected_product
            .uses_dealiased_velocity()
            .then(|| self.dealiased_velocity_readout_grid(&volume, selected_cut))
            .flatten();
        let grid = dealiased_grid.as_deref().unwrap_or(source_grid);
        let (radar_lat, radar_lon) = self.loaded_volume_location()?;
        // Probe the gate ACTUALLY RENDERED under the cursor: invert the
        // raster's screen mapping (planar ENU about the radar, rotated by
        // the AEQD convergence angle at draw time) instead of re-deriving
        // ENU from lat/lon (review finding F3).
        let radar_pos = self.lon_lat_to_screen(rect, radar_lon, radar_lat);
        let angle = self.aeqd_north_angle(rect, radar_lat, radar_lon);
        let offset = position - radar_pos;
        let (sin, cos) = (-angle).sin_cos();
        let east_px = offset.x * cos - offset.y * sin;
        let north_px = -(offset.x * sin + offset.y * cos);
        let km_per_px = 111.32 / self.map_scale;
        let lon_km = east_px * km_per_px;
        let lat_km = north_px * km_per_px;
        let range_km = lat_km.hypot(lon_km);
        let max_range_km = grid_range_km(grid)?;
        if range_km > max_range_km {
            return None;
        }

        let mut azimuth_deg = lon_km.atan2(lat_km).to_degrees();
        if azimuth_deg < 0.0 {
            azimuth_deg += 360.0;
        }
        let (row, radial_index) = nearest_grid_row(cut, grid, azimuth_deg)?;
        let gate = gate_for_range(grid, range_km)?;
        let base_value = grid
            .scaled_value(row, gate)
            .filter(|value| value.is_finite())?;
        let raw = (!selected_product.uses_dealiased_velocity())
            .then(|| grid_raw_value(grid, row, gate))
            .flatten();
        let radial = cut.radials.get(radial_index)?;
        let value = if selected_product.is_storm_relative_velocity() {
            storm_relative_velocity_mps(base_value, radial.azimuth_deg, self.current_storm_motion())
        } else {
            base_value
        };
        let storm_motion = self.current_storm_motion();
        let vrot = velocity_vrot_probe(cut, grid, row, gate, &selected_product, storm_motion);
        let load_timing = self.load_timing;
        // Beam-center height from the gate's true slant range (4/3-Earth model;
        // Doviak & Zrnić 1993, eq. 2.28b), not the screen-derived ground range.
        let slant_range_m = grid.gate_range.first_gate_m as f64
            + gate as f64 * grid.gate_range.gate_spacing_m as f64;
        let height_above_radar_m =
            radar_core::beam_height_above_radar_m(slant_range_m, cut.elevation_deg as f64) as f32;
        Some(CursorReadout {
            site_id: volume.site.id.clone(),
            volume_time_utc: volume.volume_time.with_timezone(&Utc),
            product: selected_product.clone(),
            cut: selected_cut,
            value,
            base_value: selected_product
                .is_storm_relative_velocity()
                .then_some(base_value),
            vrot,
            raw,
            row,
            gate,
            gate_spacing_m: grid.gate_range.gate_spacing_m,
            range_km,
            azimuth_deg,
            source_azimuth_deg: radial.azimuth_deg,
            elevation_deg: cut.elevation_deg,
            height_above_radar_m,
            nyquist_velocity_mps: radial.nyquist_velocity_mps,
            realtime_volume_id: load_timing.and_then(|timing| timing.realtime_volume_id),
            realtime_last_chunk_id: load_timing.and_then(|timing| timing.realtime_last_chunk_id),
            realtime_last_chunk_type: load_timing
                .and_then(|timing| timing.realtime_last_chunk_type),
        })
    }
}

/// The field texture the Field Loupe magnifies plus the inverse of the raster
/// transform used to draw it, the shared rotated-image blitter, cursor-readout
/// formatting, and the zoom-tier place-label rank helpers — the free items the
/// chrome painters above rely on, moved with them.
/// The field texture the Field Loupe magnifies plus the inverse of the raster
/// transform used to draw it (screen position → texture UV). Both variants
/// invert an existing forward transform exactly, so the loupe samples the same
/// pixels the field already draws — no CPU pixel work.
#[derive(Clone, Copy)]
pub(crate) struct LoupeSource {
    pub(crate) texture_id: egui::TextureId,
    pub(crate) kind: LoupeKind,
}

#[derive(Clone, Copy)]
pub(crate) enum LoupeKind {
    /// Planar-ENU radar raster: an axis-aligned `image_rect` (UV 0..1) rotated
    /// by `angle` about `pivot`, exactly as [`paint_rotated_image`] draws it.
    Rotated {
        image_rect: egui::Rect,
        pivot: egui::Pos2,
        angle: f32,
    },
    /// Full-viewport model raster painted 1:1 across `rect` with UV 0..1.
    Rect { rect: egui::Rect },
}

impl LoupeSource {
    /// Screen position → texture UV, the inverse of the raster's forward map.
    /// UVs outside 0..1 are fine: the NEAREST sampler clamps to the texture
    /// edge (transparent at the radar raster boundary), so the loupe degrades
    /// gracefully at the coverage edge instead of wrapping.
    pub(crate) fn screen_to_uv(&self, screen: egui::Pos2) -> egui::Pos2 {
        match self.kind {
            LoupeKind::Rotated {
                image_rect,
                pivot,
                angle,
            } => {
                // Forward: screen = pivot + R(angle) · (axis − pivot).
                // Inverse: axis = pivot + R(−angle) · (screen − pivot).
                let (sin, cos) = angle.sin_cos();
                let d = screen - pivot;
                let axis = pivot + egui::vec2(d.x * cos + d.y * sin, -d.x * sin + d.y * cos);
                egui::pos2(
                    (axis.x - image_rect.left()) / image_rect.width().max(f32::EPSILON),
                    (axis.y - image_rect.top()) / image_rect.height().max(f32::EPSILON),
                )
            }
            LoupeKind::Rect { rect } => egui::pos2(
                (screen.x - rect.left()) / rect.width().max(f32::EPSILON),
                (screen.y - rect.top()) / rect.height().max(f32::EPSILON),
            ),
        }
    }
}

/// The screen position a loupe rim point samples: offsets from the loupe
/// `center` shrink by `1/magnify` toward the `focus`, so a small patch around
/// the focus fills the whole disk (the loupe's optical zoom — "scaled toward
/// the cursor by 1/zoom"). At the center point this returns the focus exactly.
pub(crate) fn loupe_sample_screen(
    focus: egui::Pos2,
    center: egui::Pos2,
    point: egui::Pos2,
    magnify: f32,
) -> egui::Pos2 {
    focus + (point - center) / magnify.max(f32::EPSILON)
}

pub(crate) fn paint_rotated_image(
    painter: &egui::Painter,
    texture_id: egui::TextureId,
    rect: egui::Rect,
    pivot: egui::Pos2,
    angle: f32,
    tint: egui::Color32,
) {
    let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
    if angle.abs() < 1e-4 {
        painter.image(texture_id, rect, uv, tint);
        return;
    }
    let (sin, cos) = angle.sin_cos();
    let rotate = |p: egui::Pos2| -> egui::Pos2 {
        let d = p - pivot;
        pivot + egui::vec2(d.x * cos - d.y * sin, d.x * sin + d.y * cos)
    };
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];
    let uvs = [
        uv.left_top(),
        uv.right_top(),
        uv.right_bottom(),
        uv.left_bottom(),
    ];
    let mut mesh = egui::epaint::Mesh::with_texture(texture_id);
    for (corner, uv) in corners.iter().zip(uvs.iter()) {
        mesh.vertices.push(egui::epaint::Vertex {
            pos: rotate(*corner),
            uv: *uv,
            color: tint,
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

pub(crate) fn format_cursor_readout(
    readout: &CursorReadout,
    unit_system: units::Units,
    display_unit: (&str, f32),
    time_zone: DisplayTimeZone,
) -> String {
    let raw = readout
        .raw
        .map(|raw| raw.to_string())
        .unwrap_or_else(|| "-".to_owned());
    // Main value in the active table's declared unit space (see
    // `table_display_unit`); raw VEL/Nyq diagnostics deliberately stay SI.
    let (units, unit_scale) = display_unit;
    let value = if units.is_empty() {
        format!("{:.1}", readout.value)
    } else {
        format!("{:.1} {units}", readout.value / unit_scale)
    };
    let base_value = readout
        .base_value
        .map(|value| format!(" VEL {:.1} m/s", value))
        .unwrap_or_default();
    let vrot = readout
        .vrot
        .map(|probe| {
            let (velocity_unit, velocity_scale) =
                vrot_display_scale_unit(display_unit, unit_system);
            format!(
                " Vrot {:.1} {velocity_unit} dV {:.1} {velocity_unit} sep {:.2} km in r{}/g{} {:05.1} {:.1} {velocity_unit} out r{}/g{} {:05.1} {:.1} {velocity_unit}",
                probe.vrot_mps / velocity_scale,
                probe.delta_v_mps / velocity_scale,
                probe.separation_km,
                probe.inbound.row,
                probe.inbound.gate,
                probe.inbound.azimuth_deg,
                probe.inbound.value_mps / velocity_scale,
                probe.outbound.row,
                probe.outbound.gate,
                probe.outbound.azimuth_deg,
                probe.outbound.value_mps / velocity_scale
            )
        })
        .unwrap_or_default();
    let nyquist = readout
        .nyquist_velocity_mps
        .map(|nyquist| format!(" Nyq {:.1} m/s", nyquist))
        .unwrap_or_default();
    let realtime = match (
        readout.realtime_volume_id,
        readout.realtime_last_chunk_id,
        readout.realtime_last_chunk_type,
    ) {
        (Some(volume_id), Some(chunk_id), Some(chunk_type)) => {
            format!(" rt v{volume_id:03} c{chunk_id:03} {chunk_type:?}")
        }
        (Some(volume_id), Some(chunk_id), None) => {
            format!(" rt v{volume_id:03} c{chunk_id:03}")
        }
        (Some(volume_id), None, _) => format!(" rt v{volume_id:03}"),
        _ => String::new(),
    };
    let height = match unit_system {
        // Imperial keeps the m + kft dual (analyst convention).
        units::Units::Imperial => format!(
            " hgt {:.0} m ({})",
            readout.height_above_radar_m,
            units::format_beam_height(readout.height_above_radar_m, unit_system),
        ),
        units::Units::Metric => format!(" hgt {:.0} m", readout.height_above_radar_m),
    };
    format!(
        "{} {} {} cut {} {} raw {} row {} gate {} @ {} m{}{} az {:05.1} src {:05.1} range {:.1} km elev {:.2}{}{}{}",
        readout.site_id,
        time_zone.format_hms(readout.volume_time_utc),
        readout.product.label(),
        readout.cut,
        value,
        raw,
        readout.row,
        readout.gate,
        readout.gate_spacing_m,
        base_value,
        vrot,
        readout.azimuth_deg,
        readout.source_azimuth_deg,
        readout.range_km,
        readout.elevation_deg,
        height,
        nyquist,
        realtime
    )
}

pub(crate) fn world_place_label_rank(map_scale: f32) -> Option<u8> {
    if map_scale < 10.0 {
        None
    } else if map_scale < 28.0 {
        Some(0)
    } else {
        Some(1)
    }
}

fn place_label_rank(map_scale: f32) -> Option<u8> {
    if map_scale < 24.0 {
        None
    } else if map_scale < 42.0 {
        Some(0)
    } else if map_scale < 72.0 {
        Some(2)
    } else if map_scale < 130.0 {
        Some(4)
    } else if map_scale < 230.0 {
        Some(5)
    } else {
        Some(6)
    }
}
