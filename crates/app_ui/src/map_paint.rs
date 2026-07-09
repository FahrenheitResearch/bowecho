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
        // Loupe-aware placement (owner report: the card overlapped the loupe
        // when it flipped onto the disk). When the loupe is shown, re-place the
        // card so it clears the disk's bounding circle while staying on-screen.
        // No loupe → `origin` is unchanged, so this path is a no-op.
        if let Some((disk_center, disk_radius)) =
            self.cursor_loupe_disk(painter.ctx(), rect, anchor)
        {
            origin = place_inspector_card_clear_of_loupe(
                origin,
                anchor,
                egui::vec2(width, height),
                rect,
                disk_center,
                disk_radius,
            );
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

    /// The Field Loupe disk's screen geometry (`(center, radius)`) when the
    /// loupe is currently shown at `anchor`, or `None` when it is hidden. This
    /// is the SINGLE source of truth for the visibility gate and disk placement:
    /// [`Self::draw_cursor_loupe`] uses it to paint, and
    /// [`Self::draw_cursor_inspector`] uses it to keep the readout card off the
    /// disk (owner report: the card overlapped the loupe when it flipped). The
    /// disk radius is fixed ([`LOUPE_RADIUS`]) — scroll zoom changes the optical
    /// magnification, not the disk size — so the placement is independent of it.
    fn cursor_loupe_disk(
        &self,
        ctx: &egui::Context,
        rect: egui::Rect,
        anchor: egui::Pos2,
    ) -> Option<(egui::Pos2, f32)> {
        let shift_held = ctx.input(|input| input.modifiers.shift);
        if !(self.inspector_show_loupe || shift_held) {
            return None;
        }
        // v0.29.3: while the plot-domain box tool is engaged the drag must
        // not summon the magnifier mid-box (owner constraint) — Ctrl+SHIFT
        // holds Shift down, and the armed mode owns the surface outright.
        if self.plot_domain_map_drag.is_some() || self.plot_domain_arm_active() {
            return None;
        }
        if !rect.contains(anchor) {
            return None;
        }
        let radius = LOUPE_RADIUS;
        // Edge-clamp: float the disk above the cursor, flip below if it would
        // clip the top, then keep the whole disk inside the pane.
        let margin = radius + 6.0;
        let mut center = egui::pos2(anchor.x, anchor.y - (radius + 34.0));
        if center.y - radius < rect.top() + 4.0 {
            center.y = anchor.y + (radius + 34.0);
        }
        center.x = center.x.clamp(rect.left() + margin, rect.right() - margin);
        center.y = center.y.clamp(rect.top() + margin, rect.bottom() - margin);
        Some((center, radius))
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
        &mut self,
        painter: &egui::Painter,
        rect: egui::Rect,
        anchor: egui::Pos2,
        radar_has_gate: bool,
    ) {
        // Visibility gate + disk placement live in `cursor_loupe_disk` so the
        // inspector card can consult the SAME geometry and keep itself off the
        // disk. `RADIUS` is fixed; scroll changes the optical magnification.
        let Some((center, radius)) = self.cursor_loupe_disk(painter.ctx(), rect, anchor) else {
            return;
        };
        // `magnify` is the loupe's optical zoom (scroll-wheel adjustable while
        // the loupe is shown, persisted for the session): a rim vertex at screen
        // offset `d` from the loupe center samples the field at `focus + d /
        // magnify`, so a small patch around the focus fills the whole disk.
        let magnify = self.loupe_magnify;
        const RIM: usize = 72;
        let focus = anchor;

        // Native-gate path (feat/loupe-native-gates): for a plain base moment
        // under the cursor, rasterize the REAL polar gates into a small
        // disk-sized ColorImage and let the mesh sample it 1:1 — true gate
        // resolution instead of a magnified copy of the Cartesian raster. Any
        // product the native path does not cover (derived Cartesian fields,
        // storm-relative and dealiased velocity) falls back to the raster
        // magnifier below, unchanged.
        let native = radar_has_gate
            .then(|| {
                self.radar_loupe_native_prepare(painter.ctx(), rect, center, focus, radius, magnify)
            })
            .flatten();

        // Resolve the texture + a screen→UV mapper. Native bakes the
        // magnification into the ColorImage, so its mesh UV is a plain
        // normalization over the image's bounding square; the raster fall-back
        // keeps the historical inverse-with-`loupe_sample_screen` mapping.
        let (texture_id, native_rect, raster_source) = match native {
            Some((id, img_rect)) => (id, Some(img_rect), None),
            None => {
                let Some(source) = self.loupe_source(painter.ctx(), rect, anchor, radar_has_gate)
                else {
                    return;
                };
                (source.texture_id, None, Some(source))
            }
        };

        let to_uv = |q: egui::Pos2| -> egui::Pos2 {
            match (native_rect, &raster_source) {
                (Some(img_rect), _) => egui::pos2(
                    (q.x - img_rect.left()) / img_rect.width().max(f32::EPSILON),
                    (q.y - img_rect.top()) / img_rect.height().max(f32::EPSILON),
                ),
                (None, Some(source)) => {
                    source.screen_to_uv(loupe_sample_screen(focus, center, q, magnify))
                }
                (None, None) => egui::Pos2::ZERO,
            }
        };
        // Opaque backdrop UNDER the loupe image: the loupe ColorImage leaves
        // no-echo / no-data / off-grid texels TRANSPARENT, and the raster path
        // likewise has transparent (no-echo) areas. Without a backdrop the
        // non-magnified 1x map underneath bleeds through at the wrong scale —
        // the ghosted double-image the owner circled. A filled disk in the
        // loupe's own chrome dark (matching the rim halo) turns every
        // transparent texel into a clean neutral background, making the loupe a
        // self-contained magnified window. Drawn before the mesh so the mesh
        // blends on top; the rim halo below covers the boundary seam.
        painter.circle_filled(center, radius, egui::Color32::from_rgb(12, 15, 20));
        let tint = egui::Color32::WHITE;
        let mut mesh = egui::epaint::Mesh::with_texture(texture_id);
        mesh.vertices.push(egui::epaint::Vertex {
            pos: center,
            uv: to_uv(center),
            color: tint,
        });
        for i in 0..RIM {
            let theta = std::f32::consts::TAU * i as f32 / RIM as f32;
            let q = center + radius * egui::vec2(theta.cos(), theta.sin());
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
            radius,
            egui::Stroke::new(3.0, egui::Color32::from_rgb(12, 15, 20)),
        );
        painter.circle_stroke(
            center,
            radius,
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

    /// Native-gate loupe: resolve the primary product's polar gates under the
    /// cursor, rasterize the disk region into a small ColorImage (reused/rebuilt
    /// only when the sampling inputs change), upload it, and return the texture
    /// id + the image's screen-space bounding square. Returns `None` for any
    /// product the native path does not cover — derived Cartesian products,
    /// storm-relative velocity (needs per-radial motion), and dealiased velocity
    /// (a separate grid) — so the caller falls back to the raster magnifier.
    fn radar_loupe_native_prepare(
        &mut self,
        ctx: &egui::Context,
        rect: egui::Rect,
        center: egui::Pos2,
        focus: egui::Pos2,
        radius: f32,
        magnify: f32,
    ) -> Option<(egui::TextureId, egui::Rect)> {
        let dim = (2.0 * radius).ceil().max(1.0) as usize;
        let img_rect = egui::Rect::from_center_size(center, egui::vec2(dim as f32, dim as f32));

        // Resolve the sampling plan (product/grid/color table/geometry). Cheap
        // enough to run every frame; the expensive raster + upload below is
        // skipped when the plan and disk placement are unchanged.
        let plan = self.radar_loupe_native_plan(rect)?;
        let key = LoupeNativeKey::new(&plan, focus, center, magnify, dim);
        if self.loupe_native_key.as_ref() == Some(&key)
            && let Some(texture) = &self.loupe_native_texture
        {
            return Some((texture.id(), img_rect));
        }

        let grid = plan.grid()?;
        let image = build_radar_loupe_image(
            dim,
            center,
            focus,
            radius,
            magnify,
            &plan.geom,
            &plan.az_lut,
            grid,
            &plan.sampler,
        );
        match &mut self.loupe_native_texture {
            Some(texture) => texture.set(image, radar_texture_options()),
            None => {
                self.loupe_native_texture =
                    Some(ctx.load_texture("loupe-native", image, radar_texture_options()));
            }
        }
        self.loupe_native_key = Some(key);
        Some((self.loupe_native_texture.as_ref()?.id(), img_rect))
    }

    /// Everything the native loupe needs to color one polar gate under the
    /// cursor for the CURRENT primary product/cut, mirroring what the radar
    /// raster (`primary_render_request_for_volume`) and the cursor readout
    /// (`cursor_readout_for`) resolve: the same displayed volume, the same
    /// `MomentGrid`, and the same color table (`render_color_tables_for_product`
    /// → `for_family`). `None` if the product is not a plain base moment.
    fn radar_loupe_native_plan(&self, rect: egui::Rect) -> Option<LoupeNativePlan> {
        let product = self.selected_product.clone();
        // Native gates only exist for the plain base moments; the raster
        // magnifier still serves everything else.
        if product.derived().is_some()
            || product.is_storm_relative_velocity()
            || self.product_render_uses_dealiased_velocity(&product)
        {
            return None;
        }
        let volume = self
            .volume
            .clone()
            .map(|volume| self.display_source_volume_for_product(&product, volume))?;
        let cut = volume.cuts.get(self.selected_cut)?;
        let grid = cut.moments.get(&product.base_moment())?;
        if grid.radial_indices.is_empty() {
            return None;
        }
        let (radar_lat, radar_lon) = self.loaded_volume_location()?;
        let geom = LoupeGateGeom {
            radar_pos: self.lon_lat_to_screen(rect, radar_lon, radar_lat),
            angle: self.aeqd_north_angle(rect, radar_lat, radar_lon),
            km_per_px: 111.32 / self.map_scale,
            max_range_km: grid_range_km(grid)?,
        };
        let color_tables = self.render_color_tables_for_product(&product);
        let color_signature = color_tables.signature_for_family(product.color_family());
        let table = color_tables.for_family(product.color_family()).clone();
        let az_lut = LoupeAzLut::build(cut, grid);
        Some(LoupeNativePlan {
            cut: self.selected_cut,
            moment: product.base_moment(),
            product_label: product.label().to_owned(),
            color_signature,
            geom,
            az_lut,
            sampler: table.sampler(),
            volume,
        })
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

/// Field Loupe disk radius in screen points. Fixed: the scroll wheel changes
/// the optical magnification, not the disk size.
pub(crate) const LOUPE_RADIUS: f32 = 70.0;
/// Default loupe optical magnification (the historical `MAGNIFY` const).
pub(crate) const LOUPE_MAGNIFY_DEFAULT: f32 = 6.0;
/// Loupe magnification clamp — scroll cannot push it outside this range.
pub(crate) const LOUPE_MAGNIFY_MIN: f32 = 2.0;
pub(crate) const LOUPE_MAGNIFY_MAX: f32 = 20.0;
/// Per-scroll-point exponent rate: `magnify *= exp(scroll * RATE)`. One wheel
/// notch (~50 points on Windows) is ~exp(50*0.0025)=1.13x, a comfortable step.
pub(crate) const LOUPE_MAGNIFY_SCROLL_RATE: f32 = 0.0025;

/// Whether the Field Loupe owns the scroll wheel this frame (so it retunes the
/// loupe magnification and the map does NOT zoom). The loupe is shown when the
/// inspector card is enabled AND (its loupe toggle is on OR Shift is held), and
/// never while a plot-domain box drag/arm owns the surface.
pub(crate) fn loupe_owns_scroll(
    show_inspector_card: bool,
    loupe_toggle: bool,
    shift_held: bool,
    plot_domain_busy: bool,
) -> bool {
    show_inspector_card && (loupe_toggle || shift_held) && !plot_domain_busy
}

/// Next loupe magnification after a scroll of `scroll` points (wheel up =
/// positive = magnify more), applied multiplicatively so each notch is a
/// constant ratio, then clamped to `[LOUPE_MAGNIFY_MIN, LOUPE_MAGNIFY_MAX]`.
pub(crate) fn next_loupe_magnify(current: f32, scroll: f32) -> f32 {
    let factor = (scroll * LOUPE_MAGNIFY_SCROLL_RATE).exp();
    (current * factor).clamp(LOUPE_MAGNIFY_MIN, LOUPE_MAGNIFY_MAX)
}

/// Distance from `center` to the nearest point of `card` (0 when `center` is
/// inside the rectangle). The rect–circle separation test the loupe-avoiding
/// card placement is built on.
pub(crate) fn rect_center_clearance(card: egui::Rect, center: egui::Pos2) -> f32 {
    let nearest = egui::pos2(
        center.x.clamp(card.left(), card.right()),
        center.y.clamp(card.top(), card.bottom()),
    );
    (center - nearest).length()
}

/// Whether `card` intrudes within `radius` of the disk at `center` (the disk's
/// bounding circle, plus a few points of breathing room so the card never
/// kisses the rim halo).
pub(crate) fn card_overlaps_loupe(card: egui::Rect, center: egui::Pos2, radius: f32) -> bool {
    rect_center_clearance(card, center) < radius + LOUPE_CARD_PAD
}

/// Breathing room between the inspector card and the loupe disk's bounding
/// circle (covers the 3-point rim halo drawn on the disk boundary).
pub(crate) const LOUPE_CARD_PAD: f32 = 6.0;

/// Choose the inspector card's top-left so it stays on-screen AND, when the
/// loupe disk is shown, never overlaps it. `default_origin` is the card's
/// normal (loupe-unaware) placement; when it already clears the disk it is
/// returned untouched, so the no-loupe path and the common non-overlap case are
/// pixel-identical to before. Otherwise candidate placements are tried in
/// priority order — the four anchor-relative quadrants first (keep the card by
/// the cursor), then four disk-relative slots that are guaranteed to clear the
/// disk in one axis — each clamped on-screen and rejected if it still overlaps.
/// If every candidate collides (a very cramped pane) the one with the most
/// clearance is returned.
pub(crate) fn place_inspector_card_clear_of_loupe(
    default_origin: egui::Pos2,
    anchor: egui::Pos2,
    size: egui::Vec2,
    rect: egui::Rect,
    disk_center: egui::Pos2,
    disk_radius: f32,
) -> egui::Pos2 {
    let bounds = rect.shrink(4.0);
    let clamp_on_screen = |o: egui::Pos2| -> egui::Pos2 {
        egui::pos2(
            o.x.clamp(bounds.left(), (bounds.right() - size.x).max(bounds.left())),
            o.y.clamp(bounds.top(), (bounds.bottom() - size.y).max(bounds.top())),
        )
    };
    let overlaps = |o: egui::Pos2| -> bool {
        card_overlaps_loupe(egui::Rect::from_min_size(o, size), disk_center, disk_radius)
    };
    // Keep today's placement whenever it already clears the disk (the loupe-off
    // path never calls this, so nothing changes there either).
    if !overlaps(default_origin) {
        return default_origin;
    }
    const GX: f32 = 16.0;
    const GY: f32 = 14.0;
    // Push disk-relative candidates a hair past the overlap threshold so, absent
    // on-screen clamping, they land clearly outside the disk (not exactly on the
    // boundary).
    let gap = disk_radius + LOUPE_CARD_PAD + 2.0;
    let candidates = [
        // Anchor-relative quadrants (preferred — card rides near the cursor).
        egui::pos2(anchor.x + GX, anchor.y + GY),
        egui::pos2(anchor.x - GX - size.x, anchor.y + GY),
        egui::pos2(anchor.x + GX, anchor.y - GY - size.y),
        egui::pos2(anchor.x - GX - size.x, anchor.y - GY - size.y),
        // Disk-relative fallbacks (each clears the disk along one axis).
        egui::pos2(disk_center.x + gap, anchor.y - size.y * 0.5),
        egui::pos2(disk_center.x - gap - size.x, anchor.y - size.y * 0.5),
        egui::pos2(anchor.x - size.x * 0.5, disk_center.y + gap),
        egui::pos2(anchor.x - size.x * 0.5, disk_center.y - gap - size.y),
    ];
    let mut best = clamp_on_screen(default_origin);
    let mut best_clear = rect_center_clearance(egui::Rect::from_min_size(best, size), disk_center);
    for cand in candidates {
        let o = clamp_on_screen(cand);
        if !overlaps(o) {
            return o;
        }
        let clear = rect_center_clearance(egui::Rect::from_min_size(o, size), disk_center);
        if clear > best_clear {
            best = o;
            best_clear = clear;
        }
    }
    best
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

/// Native-gate Field Loupe support. The disk is rasterized on the CPU from the
/// real polar gates (approach #2): each loupe texel inverse-projects its screen
/// position to the radar's `(range, azimuth)`, finds the covering gate, and
/// colors it with the SAME table the 2D layer uses — so magnification exposes
/// true gate structure instead of magnified Cartesian raster texels.
///
/// Screen position → radar polar `(range_km, azimuth_deg)`. This is the exact
/// inverse the cursor readout uses (`cursor_readout_for`): planar ENU about the
/// radar screen position, de-rotated by the AEQD convergence `angle`, scaled by
/// `km_per_px = 111.32 / map_scale`. Returns `None` beyond the grid's range.
#[derive(Clone, Copy)]
pub(crate) struct LoupeGateGeom {
    pub(crate) radar_pos: egui::Pos2,
    pub(crate) angle: f32,
    pub(crate) km_per_px: f32,
    pub(crate) max_range_km: f32,
}

impl LoupeGateGeom {
    pub(crate) fn range_az(&self, screen: egui::Pos2) -> Option<(f32, f32)> {
        let offset = screen - self.radar_pos;
        let (sin, cos) = (-self.angle).sin_cos();
        let east_px = offset.x * cos - offset.y * sin;
        let north_px = -(offset.x * sin + offset.y * cos);
        let lon_km = east_px * self.km_per_px;
        let lat_km = north_px * self.km_per_px;
        let range_km = lat_km.hypot(lon_km);
        if range_km > self.max_range_km {
            return None;
        }
        let mut azimuth_deg = lon_km.atan2(lat_km).to_degrees();
        if azimuth_deg < 0.0 {
            azimuth_deg += 360.0;
        }
        Some((range_km, azimuth_deg))
    }
}

/// Bin count for the azimuth→row lookup (0.25°/bin — finer than any real radial
/// spacing, so the discretization is sub-radial and invisible).
const LOUPE_AZ_BINS: usize = 1440;

/// O(1) azimuth → grid-row lookup for one cut/grid, built once per loupe image.
/// Approximates [`nearest_grid_row`] (nearest radial within its angular
/// threshold) with a binned table so the per-texel loop does not rescan every
/// radial (the disk is tens of thousands of texels).
pub(crate) struct LoupeAzLut {
    bins: Box<[i32; LOUPE_AZ_BINS]>,
}

impl LoupeAzLut {
    pub(crate) fn build(cut: &ElevationCut, grid: &MomentGrid) -> Self {
        let mut bins = Box::new([-1i32; LOUPE_AZ_BINS]);
        let row_count = grid.radial_indices.len();
        if row_count == 0 {
            return Self { bins };
        }
        let threshold_deg = (360.0 / row_count as f32 * 0.55).clamp(0.35, 0.8);
        let span_bins = (threshold_deg / 360.0 * LOUPE_AZ_BINS as f32).ceil() as i32;
        let mut best_delta = [f32::INFINITY; LOUPE_AZ_BINS];
        for (row, radial_index) in grid.radial_indices.iter().enumerate() {
            let Some(radial) = cut.radials.get(*radial_index) else {
                continue;
            };
            let azimuth = radial.azimuth_deg.rem_euclid(360.0);
            let center_bin = (azimuth / 360.0 * LOUPE_AZ_BINS as f32).round() as i32;
            for delta_bin in -span_bins..=span_bins {
                let bin = (center_bin + delta_bin).rem_euclid(LOUPE_AZ_BINS as i32) as usize;
                let bin_azimuth = bin as f32 / LOUPE_AZ_BINS as f32 * 360.0;
                let delta = angle_delta_deg(bin_azimuth, azimuth);
                if delta <= threshold_deg && delta < best_delta[bin] {
                    best_delta[bin] = delta;
                    bins[bin] = row as i32;
                }
            }
        }
        Self { bins }
    }

    pub(crate) fn row(&self, azimuth_deg: f32) -> Option<usize> {
        let bin = (azimuth_deg.rem_euclid(360.0) / 360.0 * LOUPE_AZ_BINS as f32).floor() as usize;
        let bin = bin.min(LOUPE_AZ_BINS - 1);
        let row = self.bins[bin];
        (row >= 0).then_some(row as usize)
    }
}

/// The on-screen color the radar raster paints for one gate, mirroring
/// render2d's per-gate logic exactly (nodata → transparent; range-folded → the
/// table's RF color; else scaled value → table color; fully transparent color →
/// `None`) so the native loupe matches the map pixel-for-pixel. Reads the raw
/// code for U8/U16 (not [`MomentGrid::scaled_value`], which drops the
/// range-folded distinction) and the finite float directly for F32.
pub(crate) fn loupe_gate_color(
    grid: &MomentGrid,
    sampler: &color_tables::ColorSampler,
    row: usize,
    gate: usize,
) -> Option<[u8; 4]> {
    let color = match &grid.storage {
        MomentStorage::F32(_) => {
            let value = grid.scaled_value(row, gate)?;
            if !value.is_finite() {
                return None;
            }
            sampler.color_for_value(value)
        }
        MomentStorage::U8(_) | MomentStorage::U16(_) => {
            let raw = grid_raw_value(grid, row, gate)?;
            if grid.nodata == Some(raw) {
                return None;
            }
            if grid.range_folded == Some(raw) {
                sampler.range_folded_color()
            } else {
                sampler.color_for_value((raw as f32 - grid.offset) / grid.scale)
            }
        }
    };
    (color[3] != 0).then_some(color)
}

/// Rasterize the native-gate loupe disk into a `dim`×`dim` [`egui::ColorImage`]
/// laid out in DISK SCREEN space over `Rect::from_center_size(center, dim)`.
/// Each texel bakes the loupe's optical magnification via [`loupe_sample_screen`]
/// and then samples the real polar gate under that field position; texels
/// outside the disk stay transparent. The mesh samples this image with NEAREST,
/// so gate edges stay crisp at any magnification.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_radar_loupe_image(
    dim: usize,
    center: egui::Pos2,
    focus: egui::Pos2,
    radius: f32,
    magnify: f32,
    geom: &LoupeGateGeom,
    az_lut: &LoupeAzLut,
    grid: &MomentGrid,
    sampler: &color_tables::ColorSampler,
) -> egui::ColorImage {
    let mut pixels = vec![egui::Color32::TRANSPARENT; dim * dim];
    let radius_sq = radius * radius;
    // Match the mesh's `img_rect.min = center - dim/2` exactly so texel centers
    // line up with the sampled screen positions the mesh UVs address.
    let half = dim as f32 / 2.0;
    let origin = egui::pos2(center.x - half, center.y - half);
    for j in 0..dim {
        for i in 0..dim {
            let disk = egui::pos2(origin.x + i as f32 + 0.5, origin.y + j as f32 + 0.5);
            let delta = disk - center;
            if delta.x * delta.x + delta.y * delta.y > radius_sq {
                continue;
            }
            let screen = loupe_sample_screen(focus, center, disk, magnify);
            let Some((range_km, azimuth_deg)) = geom.range_az(screen) else {
                continue;
            };
            let Some(row) = az_lut.row(azimuth_deg) else {
                continue;
            };
            let Some(gate) = gate_for_range(grid, range_km) else {
                continue;
            };
            if let Some(color) = loupe_gate_color(grid, sampler, row, gate) {
                pixels[j * dim + i] =
                    egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], color[3]);
            }
        }
    }
    egui::ColorImage {
        size: [dim, dim],
        source_size: egui::vec2(dim as f32, dim as f32),
        pixels,
    }
}

/// The resolved plan for one native-gate loupe frame: which displayed volume,
/// cut, and moment to sample, plus the color sampler and screen→polar geometry.
/// Holds the volume by `Arc` (cheap) and resolves the grid by reference so a
/// per-frame rebuild never clones the gate array.
pub(crate) struct LoupeNativePlan {
    pub(crate) volume: Arc<RadarVolume>,
    pub(crate) cut: usize,
    pub(crate) moment: MomentType,
    pub(crate) product_label: String,
    pub(crate) color_signature: u64,
    pub(crate) geom: LoupeGateGeom,
    pub(crate) az_lut: LoupeAzLut,
    pub(crate) sampler: color_tables::ColorSampler,
}

impl LoupeNativePlan {
    pub(crate) fn grid(&self) -> Option<&MomentGrid> {
        self.volume.cuts.get(self.cut)?.moments.get(&self.moment)
    }
}

/// Cache key that decides whether the native loupe image must be rebuilt +
/// re-uploaded: any change to the sampled data, its coloring, or the disk's
/// screen placement invalidates it. Screen positions are quantized to whole
/// pixels so sub-pixel jitter does not thrash the GPU upload.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LoupeNativeKey {
    volume_ptr: usize,
    cut: usize,
    product_label: String,
    color_signature: u64,
    dim: usize,
    focus: [i32; 2],
    center: [i32; 2],
    radar_pos: [i32; 2],
    angle_milli: i32,
    km_per_px_milli: i32,
    magnify_milli: i32,
}

impl LoupeNativeKey {
    fn new(
        plan: &LoupeNativePlan,
        focus: egui::Pos2,
        center: egui::Pos2,
        magnify: f32,
        dim: usize,
    ) -> Self {
        let quantize = |p: egui::Pos2| [p.x.round() as i32, p.y.round() as i32];
        Self {
            volume_ptr: Arc::as_ptr(&plan.volume) as usize,
            cut: plan.cut,
            product_label: plan.product_label.clone(),
            color_signature: plan.color_signature,
            dim,
            focus: quantize(focus),
            center: quantize(center),
            radar_pos: quantize(plan.geom.radar_pos),
            angle_milli: (plan.geom.angle * 1000.0).round() as i32,
            km_per_px_milli: (plan.geom.km_per_px * 1000.0).round() as i32,
            magnify_milli: (magnify * 1000.0).round() as i32,
        }
    }
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

// Map-paint sub-move C (markers): the site / international / community /
// custom-poll / RAOB marker painters, their per-frame point-builders, the
// loaded-volume / coordinate / radar-layer marker painters, and the marker
// free-fns, moved VERBATIM out of `main.rs`. The INPUT cluster
// (`handle_*_click`, `drop_coordinate_marker_at_screen_point`,
// `show_community_feed_menu`) stays in `main.rs` and reaches the moved
// point-builders / `nearest_*_within` helpers through their `pub(crate)`
// promotions; the residual paint dispatch (`single_pane_canvas` /
// `grid_canvas`) and the `#[cfg(test)]` module reach the rest the same way.
impl ViewerApp {
    pub(crate) fn draw_site_markers(
        &self,
        painter: &egui::Painter,
        site_points: &[(usize, egui::Pos2)],
    ) {
        for (index, position) in site_points {
            let selected = *index == self.selected_site_index;
            let Some(site) = self.sites.get(*index) else {
                continue;
            };
            let fill = self.site_marker_color(site, selected);
            let radius = if selected { 5.5 } else { 3.0 };
            painter.circle_filled(*position, radius, fill);
            if selected {
                painter.circle_stroke(
                    *position,
                    10.0,
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(236, 246, 255)),
                );
            }
            // Red ring on any radar the NWS reports DOWN (only sites whose
            // status has been fetched — the selected site auto-fetches while
            // the Radar Status panel is open, clicked sites via the menu).
            if self.app_settings.show_radar_status
                && let Some(badge) = self.site_operational_down_badge(site)
            {
                painter.circle_stroke(*position, radius + 3.5, egui::Stroke::new(2.0, badge));
            }
        }

        let mut occupied = Vec::with_capacity(site_points.len().min(48));
        for (index, position) in site_points {
            let selected = *index == self.selected_site_index;
            let Some(site) = self.sites.get(*index) else {
                continue;
            };
            if !self.site_marker_should_label(site, selected) {
                continue;
            }
            self.draw_site_marker_label(painter, *position, site, selected, &mut occupied);
        }
    }

    pub(crate) fn site_marker_color(&self, site: &RadarSite, selected: bool) -> egui::Color32 {
        if let Some(scan_time) = self.loaded_site_scan_time_utc(&site.level2_id) {
            return freshness_ring_color(
                scan_time,
                Utc::now(),
                if selected { 255 } else { 220 },
                self.style_registry.radar_age(),
            );
        }
        if site_is_terminal_radar(site) {
            return egui::Color32::from_rgb(255, 202, 92);
        }
        let rings = self.style_registry.range_rings();
        style_color32(if selected {
            rings.site_selected_color
        } else {
            rings.site_idle_color
        })
    }

    fn loaded_site_scan_time_utc(&self, site_id: &str) -> Option<DateTime<Utc>> {
        if let Some(volume) = &self.volume
            && volume.site.id.eq_ignore_ascii_case(site_id)
        {
            return Some(volume.volume_time.with_timezone(&Utc));
        }
        self.radar_layers.iter().find_map(|layer| {
            let volume = layer.volume.as_ref()?;
            (volume.site.id.eq_ignore_ascii_case(site_id))
                .then(|| volume.volume_time.with_timezone(&Utc))
        })
    }

    pub(crate) fn site_marker_should_label(&self, site: &RadarSite, selected: bool) -> bool {
        if !self.app_settings.show_radar_labels {
            return false;
        }
        selected
            || if site_is_terminal_radar(site) {
                self.map_scale >= TERMINAL_SITE_LABEL_MIN_SCALE
            } else {
                self.map_scale >= SITE_LABEL_MIN_SCALE
            }
    }

    pub(crate) fn selected_radar_label_fill(&self) -> egui::Color32 {
        let fresh = style_color32(self.style_registry.radar_age().fresh_color);
        egui::Color32::from_rgba_unmultiplied(fresh.r(), fresh.g(), fresh.b(), fresh.a().max(224))
    }

    pub(crate) fn selected_radar_label_stroke(&self) -> egui::Color32 {
        let fresh = style_color32(self.style_registry.radar_age().fresh_color);
        egui::Color32::from_rgba_unmultiplied(
            fresh.r().saturating_add(42),
            fresh.g().saturating_add(12),
            fresh.b().saturating_add(42),
            255,
        )
    }

    pub(crate) fn selected_radar_label_text_color() -> egui::Color32 {
        egui::Color32::WHITE
    }

    fn site_marker_label_parts(
        &self,
        site: &RadarSite,
        selected: bool,
    ) -> (RadarLabelStyle, String, f32, egui::Color32, egui::Vec2) {
        let terminal = site_is_terminal_radar(site);
        let style = RadarLabelStyle::from_key(&self.app_settings.radar_label_style);
        let label = match style {
            RadarLabelStyle::FullBox | RadarLabelStyle::Text => site_marker_label(site),
            RadarLabelStyle::IdBox => site.level2_id.clone(),
        };
        let font_px = if selected { 13.0 } else { 11.0 };
        let text_color = match style {
            RadarLabelStyle::FullBox if terminal => egui::Color32::from_rgb(25, 18, 6),
            RadarLabelStyle::IdBox if terminal => egui::Color32::from_rgb(255, 232, 168),
            _ => egui::Color32::from_rgb(238, 246, 255),
        };
        let padding = match style {
            RadarLabelStyle::Text => egui::vec2(8.0, 4.0),
            RadarLabelStyle::IdBox => egui::vec2(10.0, 5.0),
            RadarLabelStyle::FullBox => egui::vec2(8.0, 4.0),
        };
        (style, label, font_px, text_color, padding)
    }

    pub(crate) fn site_marker_label_rect(
        &self,
        position: egui::Pos2,
        site: &RadarSite,
        selected: bool,
    ) -> egui::Rect {
        let (_style, label, font_px, _text_color, padding) =
            self.site_marker_label_parts(site, selected);
        let terminal = site_is_terminal_radar(site);
        let text_width = label.chars().count() as f32 * font_px * 0.64;
        let text_height = font_px * 1.25;
        egui::Rect::from_min_size(
            position + egui::vec2(10.0, if terminal { -15.0 } else { -12.0 }),
            egui::vec2(text_width, text_height) + padding,
        )
    }

    pub(crate) fn nearest_site_marker_within(
        &self,
        site_points: &[(usize, egui::Pos2)],
        pointer: egui::Pos2,
    ) -> Option<(usize, f32)> {
        let marker_hit = nearest_marker_within(site_points, pointer);
        let mut occupied = Vec::with_capacity(site_points.len().min(48));
        let label_hit = site_points
            .iter()
            .filter_map(|(index, position)| {
                let selected = *index == self.selected_site_index;
                let site = self.sites.get(*index)?;
                if !self.site_marker_should_label(site, selected) {
                    return None;
                }
                let rect = self.site_marker_label_rect(*position, site, selected);
                if !selected
                    && occupied
                        .iter()
                        .any(|existing: &egui::Rect| existing.intersects(rect.expand(2.0)))
                {
                    return None;
                }
                occupied.push(rect);
                rect.expand(2.0)
                    .contains(pointer)
                    .then_some((*index, 0.0_f32))
            })
            .min_by(|left, right| left.1.total_cmp(&right.1));

        match (marker_hit, label_hit) {
            (Some(marker), Some(label)) => {
                if marker.1 <= label.1 {
                    Some(marker)
                } else {
                    Some(label)
                }
            }
            (Some(marker), None) => Some(marker),
            (None, Some(label)) => Some(label),
            (None, None) => None,
        }
    }

    fn draw_site_marker_label(
        &self,
        painter: &egui::Painter,
        position: egui::Pos2,
        site: &RadarSite,
        selected: bool,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let (style, label, font_px, mut text_color, _padding) =
            self.site_marker_label_parts(site, selected);
        if selected {
            text_color = match style {
                RadarLabelStyle::Text => self.selected_radar_label_stroke(),
                RadarLabelStyle::IdBox | RadarLabelStyle::FullBox => {
                    Self::selected_radar_label_text_color()
                }
            };
        }
        let galley = painter.layout_no_wrap(
            label.clone(),
            egui::FontId::proportional(font_px),
            text_color,
        );
        let terminal = site_is_terminal_radar(site);
        let rect = self.site_marker_label_rect(position, site, selected);
        if !selected
            && occupied
                .iter()
                .any(|existing| existing.intersects(rect.expand(2.0)))
        {
            return;
        }
        match style {
            RadarLabelStyle::Text => {
                draw_halo_text(
                    painter,
                    rect.left_center(),
                    egui::Align2::LEFT_CENTER,
                    &label,
                    egui::FontId::proportional(font_px),
                    text_color,
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 220),
                );
            }
            RadarLabelStyle::IdBox => {
                let radius = rect.height() * 0.5;
                let fill = if selected {
                    self.selected_radar_label_fill()
                } else {
                    egui::Color32::from_rgba_unmultiplied(2, 4, 7, 204)
                };
                painter.rect_filled(rect, radius, fill);
                let outline = if selected {
                    self.selected_radar_label_stroke()
                } else if terminal {
                    egui::Color32::from_rgb(255, 202, 92)
                } else {
                    egui::Color32::from_rgb(152, 176, 196)
                };
                painter.rect_stroke(
                    rect,
                    radius,
                    egui::Stroke::new(if selected { 1.7 } else { 1.2 }, outline),
                    egui::StrokeKind::Outside,
                );
                painter.galley(rect.min + egui::vec2(5.0, 2.5), galley, text_color);
            }
            RadarLabelStyle::FullBox => {
                let bg = if selected {
                    self.selected_radar_label_fill()
                } else if terminal {
                    egui::Color32::from_rgba_unmultiplied(255, 202, 92, 218)
                } else {
                    egui::Color32::from_rgba_unmultiplied(3, 6, 10, 168)
                };
                painter.rect_filled(rect, 3.0, bg);
                if selected {
                    painter.rect_stroke(
                        rect,
                        3.0,
                        egui::Stroke::new(1.5, self.selected_radar_label_stroke()),
                        egui::StrokeKind::Outside,
                    );
                }
                painter.galley(rect.min + egui::vec2(4.0, 2.0), galley, text_color);
            }
        }
        occupied.push(rect);
    }

    fn intl_site_marker_active(&self, site: &data_source::international::IntlSite) -> bool {
        if let Some(volume) = &self.volume {
            let volume_id = volume.site.id.as_str();
            return volume_id.eq_ignore_ascii_case(&site.site_id)
                || volume_id.eq_ignore_ascii_case(&site.label);
        }
        matches!(
            &self.primary.feed,
            FeedSource::Live(SiteRef::Intl { provider_id, site_id })
                if self.poll_active
                    && provider_id == site.provider_id
                    && site_id.eq_ignore_ascii_case(&site.site_id)
        )
    }

    fn intl_site_marker_should_label(&self, selected: bool) -> bool {
        self.app_settings.show_radar_labels && (selected || self.map_scale >= SITE_LABEL_MIN_SCALE)
    }

    fn intl_site_marker_label_parts(
        &self,
        site: &data_source::international::IntlSite,
        selected: bool,
    ) -> (RadarLabelStyle, String, f32, egui::Color32, egui::Vec2) {
        let style = RadarLabelStyle::from_key(&self.app_settings.radar_label_style);
        let label = match style {
            RadarLabelStyle::IdBox => site.site_id.to_ascii_uppercase(),
            RadarLabelStyle::FullBox | RadarLabelStyle::Text => site.label.clone(),
        };
        let font_px = if selected { 13.0 } else { 11.0 };
        let text_color = match style {
            RadarLabelStyle::FullBox => egui::Color32::from_rgb(26, 20, 7),
            RadarLabelStyle::IdBox => egui::Color32::from_rgb(255, 232, 168),
            RadarLabelStyle::Text => egui::Color32::from_rgb(255, 235, 198),
        };
        let padding = match style {
            RadarLabelStyle::Text => egui::vec2(8.0, 4.0),
            RadarLabelStyle::IdBox => egui::vec2(10.0, 5.0),
            RadarLabelStyle::FullBox => egui::vec2(8.0, 4.0),
        };
        (style, label, font_px, text_color, padding)
    }

    pub(crate) fn intl_site_marker_label_rect(
        &self,
        position: egui::Pos2,
        site: &data_source::international::IntlSite,
        selected: bool,
    ) -> egui::Rect {
        let (_style, label, font_px, _text_color, padding) =
            self.intl_site_marker_label_parts(site, selected);
        let text_width = label.chars().count() as f32 * font_px * 0.64;
        let text_height = font_px * 1.25;
        egui::Rect::from_min_size(
            position + egui::vec2(10.0, -12.0),
            egui::vec2(text_width, text_height) + padding,
        )
    }

    pub(crate) fn nearest_intl_marker_within(
        &self,
        intl_points: &[(usize, egui::Pos2)],
        pointer: egui::Pos2,
    ) -> Option<(usize, f32)> {
        let marker_hit = nearest_marker_within(intl_points, pointer);
        let sites = data_source::international::intl_static_sites();
        let mut occupied = Vec::with_capacity(intl_points.len().min(48));
        let label_hit = intl_points
            .iter()
            .filter_map(|(index, position)| {
                let site = sites.get(*index)?;
                let selected = self.intl_site_marker_active(site);
                if !self.intl_site_marker_should_label(selected) {
                    return None;
                }
                let rect = self.intl_site_marker_label_rect(*position, site, selected);
                if !selected
                    && occupied
                        .iter()
                        .any(|existing: &egui::Rect| existing.intersects(rect.expand(2.0)))
                {
                    return None;
                }
                occupied.push(rect);
                rect.expand(2.0)
                    .contains(pointer)
                    .then_some((*index, 0.0_f32))
            })
            .min_by(|left, right| left.1.total_cmp(&right.1));

        match (marker_hit, label_hit) {
            (Some(marker), Some(label)) => {
                if marker.1 <= label.1 {
                    Some(marker)
                } else {
                    Some(label)
                }
            }
            (Some(marker), None) => Some(marker),
            (None, Some(label)) => Some(label),
            (None, None) => None,
        }
    }

    fn draw_intl_site_marker_label(
        &self,
        painter: &egui::Painter,
        position: egui::Pos2,
        site: &data_source::international::IntlSite,
        selected: bool,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let (style, label, font_px, mut text_color, _padding) =
            self.intl_site_marker_label_parts(site, selected);
        if selected {
            text_color = match style {
                RadarLabelStyle::Text => self.selected_radar_label_stroke(),
                RadarLabelStyle::IdBox | RadarLabelStyle::FullBox => {
                    Self::selected_radar_label_text_color()
                }
            };
        }
        let rect = self.intl_site_marker_label_rect(position, site, selected);
        if !selected
            && occupied
                .iter()
                .any(|existing| existing.intersects(rect.expand(2.0)))
        {
            return;
        }
        match style {
            RadarLabelStyle::Text => {
                draw_halo_text(
                    painter,
                    rect.left_center(),
                    egui::Align2::LEFT_CENTER,
                    &label,
                    egui::FontId::proportional(font_px),
                    text_color,
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 220),
                );
            }
            RadarLabelStyle::IdBox => {
                let radius = rect.height() * 0.5;
                let fill = if selected {
                    self.selected_radar_label_fill()
                } else {
                    egui::Color32::from_rgba_unmultiplied(2, 4, 7, 204)
                };
                painter.rect_filled(rect, radius, fill);
                painter.rect_stroke(
                    rect,
                    radius,
                    egui::Stroke::new(
                        if selected { 1.7 } else { 1.2 },
                        if selected {
                            self.selected_radar_label_stroke()
                        } else {
                            egui::Color32::from_rgb(196, 156, 84)
                        },
                    ),
                    egui::StrokeKind::Outside,
                );
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    &label,
                    egui::FontId::proportional(font_px),
                    text_color,
                );
            }
            RadarLabelStyle::FullBox => {
                let fill = if selected {
                    self.selected_radar_label_fill()
                } else {
                    egui::Color32::from_rgba_unmultiplied(255, 202, 92, 218)
                };
                painter.rect_filled(rect, 3.0, fill);
                if selected {
                    painter.rect_stroke(
                        rect,
                        3.0,
                        egui::Stroke::new(1.5, self.selected_radar_label_stroke()),
                        egui::StrokeKind::Outside,
                    );
                }
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    &label,
                    egui::FontId::proportional(font_px),
                    text_color,
                );
            }
        }
        occupied.push(rect);
    }

    /// Screen positions of the international static-catalog markers inside
    /// `rect` — the same viewport culling as the CONUS `site_points` pass,
    /// so a CONUS view never pays for (or shows) markers a continent away.
    /// Indices key into [`data_source::international::intl_static_sites`],
    /// the providers' embedded tables: pure data, never the network, safe
    /// to walk on the UI thread every frame.
    pub(crate) fn primary_level2_site_points(&self, rect: egui::Rect) -> Vec<(usize, egui::Pos2)> {
        self.sites
            .iter()
            .enumerate()
            // Home-catalog markers: the NEXRAD/TDWR program. Research
            // feeds draw their own marker pass (explicit kind gate).
            .filter(|(_, site)| match us_site_kind(site) {
                SiteKind::Wsr88d | SiteKind::Tdwr => true,
                SiteKind::Research | SiteKind::Intl { .. } => false,
            })
            .filter_map(|(index, site)| {
                let (latitude_deg, longitude_deg) = site_location(site)?;
                let position = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg);
                rect.expand(18.0)
                    .contains(position)
                    .then_some((index, position))
            })
            .collect()
    }

    pub(crate) fn intl_site_points(&self, rect: egui::Rect) -> Vec<(usize, egui::Pos2)> {
        data_source::international::intl_static_sites()
            .iter()
            .enumerate()
            .filter_map(|(index, site)| {
                let (Some(latitude_deg), Some(longitude_deg)) =
                    (site.latitude_deg, site.longitude_deg)
                else {
                    return None;
                };
                let position = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg);
                rect.expand(18.0)
                    .contains(position)
                    .then_some((index, position))
            })
            .collect()
    }

    /// Screen positions of the RAOB launch-site markers inside `rect` —
    /// the same viewport culling as the other marker passes. Indices key
    /// into [`Self::raob_marker_sites`] (the embedded table until the
    /// session fetch lands): pure data, never the network, safe on the
    /// UI thread every frame. Empty unless the layer is toggled on.
    pub(crate) fn raob_site_points(&self, rect: egui::Rect) -> Vec<(usize, egui::Pos2)> {
        if !self.raob_markers_enabled {
            return Vec::new();
        }
        self.raob_marker_sites()
            .iter()
            .enumerate()
            .filter_map(|(index, site)| {
                let position = self.lon_lat_to_screen(rect, site.lon, site.lat);
                rect.expand(18.0)
                    .contains(position)
                    .then_some((index, position))
            })
            .collect()
    }

    /// International site markers: the same visual grammar as
    /// [`Self::draw_site_markers`] (dot + selected halo + label, identical
    /// culling and label restraint) but hollow and warm-hued so the two
    /// catalogs read apart at a glance. The actively polled international
    /// site gets the selected treatment; hovering any other marker shows a
    /// "click to live-poll" chip.
    pub(crate) fn draw_intl_site_markers(
        &self,
        painter: &egui::Painter,
        intl_points: &[(usize, egui::Pos2)],
        hovered: Option<usize>,
    ) {
        if intl_points.is_empty() {
            return;
        }
        let sites = data_source::international::intl_static_sites();
        // Warm amber against the cool CONUS site dots.
        const INTL_IDLE: egui::Color32 = egui::Color32::from_rgb(196, 156, 84);
        const INTL_LIT: egui::Color32 = egui::Color32::from_rgb(255, 214, 130);
        for (index, position) in intl_points {
            let Some(site) = sites.get(*index) else {
                continue;
            };
            let is_active = self.intl_site_marker_active(site);
            let is_hovered = hovered == Some(*index);
            if is_active {
                painter.circle_filled(*position, 5.5, INTL_LIT);
                painter.circle_stroke(
                    *position,
                    10.0,
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 240, 214)),
                );
            } else {
                // Hollow ring: selectable, but not the live poll target.
                painter.circle_stroke(
                    *position,
                    if is_hovered { 4.5 } else { 3.0 },
                    egui::Stroke::new(1.5, if is_hovered { INTL_LIT } else { INTL_IDLE }),
                );
            }
            if is_hovered && !is_active {
                let text = format!(
                    "{} · {} — click to live-poll",
                    site.label,
                    intl_provider_label(site.provider_id)
                );
                // Same chip idiom as draw_mode_chip (width from char count).
                let width = 12.0 + text.chars().count() as f32 * 6.6;
                let chip = egui::Rect::from_min_size(
                    *position + egui::vec2(10.0, -26.0),
                    egui::vec2(width, 18.0),
                );
                painter.rect_filled(
                    chip,
                    4.0,
                    egui::Color32::from_rgba_unmultiplied(16, 22, 30, 230),
                );
                painter.text(
                    chip.center(),
                    egui::Align2::CENTER_CENTER,
                    &text,
                    egui::FontId::proportional(11.0),
                    egui::Color32::from_rgb(238, 226, 200),
                );
            }
        }
        let mut occupied = Vec::with_capacity(intl_points.len().min(48));
        for (index, position) in intl_points {
            let Some(site) = sites.get(*index) else {
                continue;
            };
            let is_active = self.intl_site_marker_active(site);
            if self.intl_site_marker_should_label(is_active) {
                self.draw_intl_site_marker_label(
                    painter,
                    *position,
                    site,
                    is_active,
                    &mut occupied,
                );
            }
        }
    }

    /// Screen positions of the community research-feed markers inside
    /// `rect` — the same viewport culling as the CONUS and international
    /// passes. Indices key into
    /// [`data_source::community_feeds::community_markers`]: the embedded
    /// table, pure data, never the network, safe on the UI thread every
    /// frame.
    pub(crate) fn community_site_points(&self, rect: egui::Rect) -> Vec<(usize, egui::Pos2)> {
        data_source::community_feeds::community_markers()
            .iter()
            .enumerate()
            .filter_map(|(index, marker)| {
                let position =
                    self.lon_lat_to_screen(rect, marker.longitude_deg, marker.latitude_deg);
                rect.expand(18.0)
                    .contains(position)
                    .then_some((index, position))
            })
            .collect()
    }

    /// The community feed URL the shared poller is actively following, if
    /// any — `poll_url` is what the live tick reads, so a marker lights up
    /// exactly when its feed is the one being polled.
    pub(crate) fn active_community_poll_url(&self) -> Option<&str> {
        (self.poll_active && matches!(self.primary.feed, FeedSource::CustomUrl(_)))
            .then_some(self.poll_url.as_str())
    }

    pub(crate) fn start_custom_poll_link(&mut self, index: usize) {
        let Some(entry) = self.app_settings.custom_poll_links.get(index).cloned() else {
            return;
        };
        let label = custom_poll_entry_label(&entry);
        self.status = format!("Polling {label} · custom feed");
        self.start_known_feed_poll(&entry.poll_url);
    }

    /// Screen positions of user-saved custom radar poll markers. These are
    /// operator-owned entries from the DATA tab: no network, just persisted
    /// lat/lon plus a GR2A-style poll root.
    pub(crate) fn custom_poll_points(&self, rect: egui::Rect) -> Vec<(usize, egui::Pos2)> {
        self.app_settings
            .custom_poll_links
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if custom_poll_entry_matches_community_feed(entry) {
                    return None;
                }
                let (latitude_deg, longitude_deg) = custom_poll_entry_lat_lon(entry)?;
                let position = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg);
                rect.expand(18.0)
                    .contains(position)
                    .then_some((index, position))
            })
            .collect()
    }

    /// User custom-feed markers: red dots/rings so private IP/direct feeds
    /// are visually distinct from CONUS dots, amber international rings,
    /// and teal community research feeds.
    pub(crate) fn draw_custom_poll_markers(
        &self,
        painter: &egui::Painter,
        custom_points: &[(usize, egui::Pos2)],
        hovered: Option<usize>,
    ) {
        if custom_points.is_empty() {
            return;
        }
        let active_url = self.active_community_poll_url();
        const CUSTOM_IDLE: egui::Color32 = egui::Color32::from_rgb(220, 70, 72);
        const CUSTOM_LIT: egui::Color32 = egui::Color32::from_rgb(255, 116, 118);
        for (index, position) in custom_points {
            let Some(entry) = self.app_settings.custom_poll_links.get(*index) else {
                continue;
            };
            let label = custom_poll_entry_label(entry);
            let is_active = active_url.is_some_and(|url| poll_urls_match(url, &entry.poll_url));
            let is_hovered = hovered == Some(*index);
            if is_active {
                painter.circle_filled(*position, 5.5, CUSTOM_LIT);
                painter.circle_stroke(
                    *position,
                    10.0,
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 210, 210)),
                );
                painter.text(
                    *position + egui::vec2(12.0, -10.0),
                    egui::Align2::LEFT_CENTER,
                    &label,
                    egui::FontId::proportional(13.0),
                    egui::Color32::from_rgb(255, 224, 224),
                );
            } else {
                painter.circle_stroke(
                    *position,
                    if is_hovered { 4.8 } else { 3.4 },
                    egui::Stroke::new(1.7, if is_hovered { CUSTOM_LIT } else { CUSTOM_IDLE }),
                );
                painter.circle_filled(
                    *position,
                    1.8,
                    if is_hovered { CUSTOM_LIT } else { CUSTOM_IDLE },
                );
            }
            if is_hovered && !is_active {
                let site_id = entry.site_id.trim();
                let text = if site_id.is_empty() {
                    format!("{label} · custom feed — click to live-poll")
                } else {
                    format!("{site_id} {label} · custom feed — click to live-poll")
                };
                let width = 12.0 + text.chars().count() as f32 * 6.6;
                let chip = egui::Rect::from_min_size(
                    *position + egui::vec2(10.0, -26.0),
                    egui::vec2(width, 18.0),
                );
                painter.rect_filled(
                    chip,
                    4.0,
                    egui::Color32::from_rgba_unmultiplied(16, 22, 30, 230),
                );
                painter.text(
                    chip.center(),
                    egui::Align2::CENTER_CENTER,
                    &text,
                    egui::FontId::proportional(11.0),
                    egui::Color32::from_rgb(255, 224, 224),
                );
            }
        }
    }

    /// Community research-feed markers. Same
    /// visual grammar as [`Self::draw_intl_site_markers`] (hollow ring,
    /// hover chip, lit-when-polled) but cool teal against the warm amber
    /// international rings and the filled CONUS dots, so the catalogs
    /// catalogs read apart at a glance.
    pub(crate) fn draw_community_site_markers(
        &self,
        painter: &egui::Painter,
        community_points: &[(usize, egui::Pos2)],
        hovered: Option<usize>,
    ) {
        if community_points.is_empty() {
            return;
        }
        let markers = data_source::community_feeds::community_markers();
        let feeds = data_source::community_feeds::community_feeds();
        let active_url = self.active_community_poll_url();
        // Cool teal against the warm amber intl rings (research radars).
        const COMMUNITY_IDLE: egui::Color32 = egui::Color32::from_rgb(70, 168, 172);
        const COMMUNITY_LIT: egui::Color32 = egui::Color32::from_rgb(126, 238, 234);
        for (index, position) in community_points {
            let Some(marker) = markers.get(*index) else {
                continue;
            };
            // Which of the marker's feeds (one, or eight on the shared
            // pad) the poller is following, if any.
            let active_feed = marker.feed_indices.iter().find_map(|&feed_index| {
                let feed = feeds.get(feed_index)?;
                active_url
                    .is_some_and(|url| poll_urls_match(url, feed.poll_url))
                    .then_some(feed)
            });
            let is_hovered = hovered == Some(*index);
            if let Some(feed) = active_feed {
                painter.circle_filled(*position, 5.5, COMMUNITY_LIT);
                painter.circle_stroke(
                    *position,
                    10.0,
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(214, 252, 250)),
                );
                painter.text(
                    *position + egui::vec2(12.0, -10.0),
                    egui::Align2::LEFT_CENTER,
                    feed.id,
                    egui::FontId::proportional(13.0),
                    egui::Color32::from_rgb(224, 252, 250),
                );
            } else {
                // Hollow ring: selectable, but not the live poll target.
                painter.circle_stroke(
                    *position,
                    if is_hovered { 4.5 } else { 3.0 },
                    egui::Stroke::new(
                        1.5,
                        if is_hovered {
                            COMMUNITY_LIT
                        } else {
                            COMMUNITY_IDLE
                        },
                    ),
                );
            }
            if is_hovered && active_feed.is_none() {
                let text = if marker.feed_indices.len() > 1 {
                    format!(
                        "{} · {} research feeds — click to choose",
                        marker.label,
                        marker.feed_indices.len()
                    )
                } else {
                    format!("{} · research feed — click to live-poll", marker.label)
                };
                // Same chip idiom as draw_mode_chip (width from char count).
                let width = 12.0 + text.chars().count() as f32 * 6.6;
                let chip = egui::Rect::from_min_size(
                    *position + egui::vec2(10.0, -26.0),
                    egui::vec2(width, 18.0),
                );
                painter.rect_filled(
                    chip,
                    4.0,
                    egui::Color32::from_rgba_unmultiplied(16, 22, 30, 230),
                );
                painter.text(
                    chip.center(),
                    egui::Align2::CENTER_CENTER,
                    &text,
                    egui::FontId::proportional(11.0),
                    egui::Color32::from_rgb(208, 244, 242),
                );
            }
        }
    }

    /// RAOB launch-site markers. Same visual
    /// grammar as [`Self::draw_intl_site_markers`] (hollow + hover chip,
    /// identical culling) but a lavender hollow DIAMOND — shape and hue
    /// both read apart from the filled CONUS dots, the warm amber intl
    /// rings, and the cool teal community rings. Clicking fetches that
    /// station's observed sounding at the displayed radar time.
    pub(crate) fn draw_raob_site_markers(
        &self,
        painter: &egui::Painter,
        raob_points: &[(usize, egui::Pos2)],
        hovered: Option<usize>,
    ) {
        if raob_points.is_empty() {
            return;
        }
        let sites = self.raob_marker_sites();
        // Lavender against the amber intl and teal community rings.
        const RAOB_IDLE: egui::Color32 = egui::Color32::from_rgb(164, 138, 218);
        const RAOB_LIT: egui::Color32 = egui::Color32::from_rgb(216, 196, 255);
        for (index, position) in raob_points {
            let Some(site) = sites.get(*index) else {
                continue;
            };
            let is_hovered = hovered == Some(*index);
            let (radius, color) = if is_hovered {
                (5.5, RAOB_LIT)
            } else {
                (4.0, RAOB_IDLE)
            };
            // Hollow diamond: the launch pad, not a radar.
            painter.add(egui::Shape::closed_line(
                vec![
                    *position + egui::vec2(0.0, -radius),
                    *position + egui::vec2(radius, 0.0),
                    *position + egui::vec2(0.0, radius),
                    *position + egui::vec2(-radius, 0.0),
                ],
                egui::Stroke::new(1.5, color),
            ));
            if is_hovered {
                let text = format!(
                    "{} {} · RAOB — click for the sounding at the displayed time",
                    site.id, site.name
                );
                // Same chip idiom as draw_mode_chip (width from char count).
                let width = 12.0 + text.chars().count() as f32 * 6.6;
                let chip = egui::Rect::from_min_size(
                    *position + egui::vec2(10.0, -26.0),
                    egui::vec2(width, 18.0),
                );
                painter.rect_filled(
                    chip,
                    4.0,
                    egui::Color32::from_rgba_unmultiplied(16, 22, 30, 230),
                );
                painter.text(
                    chip.center(),
                    egui::Align2::CENTER_CENTER,
                    &text,
                    egui::FontId::proportional(11.0),
                    egui::Color32::from_rgb(232, 222, 252),
                );
            }
        }
    }

    pub(crate) fn draw_loaded_volume_marker(&self, painter: &egui::Painter, rect: egui::Rect) {
        let Some(volume) = &self.volume else {
            return;
        };
        let Some((latitude_deg, longitude_deg)) = self.loaded_volume_location() else {
            return;
        };
        let position = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg);
        if !rect.expand(18.0).contains(position) {
            return;
        }

        painter.circle_filled(position, 6.0, egui::Color32::from_rgb(88, 230, 245));
        painter.circle_stroke(
            position,
            11.0,
            egui::Stroke::new(1.8, egui::Color32::from_rgb(244, 252, 255)),
        );
        self.draw_radar_age_glyph_arc(
            painter,
            position,
            volume.volume_time.with_timezone(&Utc),
            255,
        );
        if self.loaded_volume_marker_should_draw_text(&volume.site.id) {
            painter.text(
                position + egui::vec2(12.0, -10.0),
                egui::Align2::LEFT_CENTER,
                &volume.site.id,
                egui::FontId::proportional(13.0),
                egui::Color32::from_rgb(244, 252, 255),
            );
        }
    }

    pub(crate) fn draw_coordinate_marker(&self, painter: &egui::Painter, rect: egui::Rect) {
        let Some(marker) = &self.coordinate_marker else {
            return;
        };
        let position = self.lon_lat_to_screen(rect, marker.lon, marker.lat);
        if !rect.expand(24.0).contains(position) {
            return;
        }

        let fill = egui::Color32::from_rgb(255, 215, 72);
        let outline = egui::Color32::from_rgb(15, 18, 24);
        painter.circle_filled(position, 5.5, fill);
        painter.circle_stroke(position, 8.5, egui::Stroke::new(2.0, outline));
        painter.circle_stroke(
            position,
            11.0,
            egui::Stroke::new(1.2, egui::Color32::from_rgb(255, 245, 170)),
        );
        painter.text(
            position + egui::vec2(12.0, -10.0),
            egui::Align2::LEFT_CENTER,
            coordinate_marker_label(marker),
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgb(255, 245, 210),
        );
    }

    pub(crate) fn loaded_volume_marker_should_draw_text(&self, site_id: &str) -> bool {
        let Some(site) = self.sites.get(self.selected_site_index) else {
            return true;
        };
        !site.level2_id.eq_ignore_ascii_case(site_id) || !self.site_marker_should_label(site, true)
    }

    pub(crate) fn draw_radar_layer_markers(&self, painter: &egui::Painter, rect: egui::Rect) {
        for layer in &self.radar_layers {
            if !layer.visible {
                continue;
            }
            let Some((latitude_deg, longitude_deg)) = layer.radar_location() else {
                continue;
            };
            let position = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg);
            if !rect.expand(18.0).contains(position) {
                continue;
            }
            let color = egui::Color32::from_rgba_unmultiplied(88, 190, 245, layer.opacity);
            painter.circle_filled(position, 4.5, color);
            painter.circle_stroke(
                position,
                8.5,
                egui::Stroke::new(
                    1.3,
                    egui::Color32::from_rgba_unmultiplied(214, 242, 255, layer.opacity),
                ),
            );
            if let Some(volume) = &layer.volume {
                self.draw_radar_age_glyph_arc(
                    painter,
                    position,
                    volume.volume_time.with_timezone(&Utc),
                    layer.opacity,
                );
            }
            painter.text(
                position + egui::vec2(10.0, 10.0),
                egui::Align2::LEFT_CENTER,
                &layer.site.level2_id,
                egui::FontId::proportional(11.0),
                egui::Color32::from_rgba_unmultiplied(214, 242, 255, layer.opacity),
            );
        }
    }

    fn draw_radar_age_glyph_arc(
        &self,
        painter: &egui::Painter,
        position: egui::Pos2,
        volume_time_utc: DateTime<Utc>,
        alpha: u8,
    ) {
        let age_style = self.style_registry.radar_age();
        if !age_style.glyph_arc_enabled {
            return;
        }
        let now_utc = Utc::now();
        let points = radar_age_glyph_arc_points(position, volume_time_utc, now_utc, age_style);
        if points.len() < 2 {
            return;
        }
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(
                2.5,
                freshness_ring_color(volume_time_utc, now_utc, alpha, age_style),
            ),
        ));
    }
}

pub(crate) fn marker_style(color: styles::Rgba, size_px: f32) -> styles::MarkerStyleOverride {
    styles::MarkerStyleOverride {
        color: Some(color),
        size_px: Some(size_px),
        ..Default::default()
    }
}

pub(crate) fn coordinate_marker_coordinates(marker: &CoordinateMarker) -> String {
    format!("{:.4}, {:.4}", marker.lat, marker.lon)
}

pub(crate) fn coordinate_marker_label(marker: &CoordinateMarker) -> String {
    marker
        .label
        .clone()
        .unwrap_or_else(|| coordinate_marker_coordinates(marker))
}

pub(crate) fn coordinate_marker_status_label(marker: &CoordinateMarker) -> String {
    if let Some(label) = marker.label.as_deref() {
        format!("{label} ({})", coordinate_marker_coordinates(marker))
    } else {
        coordinate_marker_coordinates(marker)
    }
}

pub(crate) fn place_search_context_for_lon_lat(lat: f32, lon: f32) -> Option<&'static str> {
    if let Some(state) = us_state_abbr_for_lon_lat(lat, lon) {
        return Some(state);
    }
    if bbox_contains(basemap_data::BASEMAP_CANADA_BOUNDS, lon, lat) {
        Some("Canada")
    } else if bbox_contains(basemap_data::BASEMAP_MEXICO_BOUNDS, lon, lat) {
        Some("Mexico")
    } else if bbox_contains(basemap_data::BASEMAP_JAPAN_BOUNDS, lon, lat) {
        Some("Japan")
    } else {
        None
    }
}

pub(crate) fn us_state_abbr_for_lon_lat(lat: f32, lon: f32) -> Option<&'static str> {
    if lat >= 50.0 && (lon <= -130.0 || lon >= 170.0) {
        return Some("AK");
    }
    if (18.5..=23.0).contains(&lat) && (-161.0..=-154.0).contains(&lon) {
        return Some("HI");
    }

    let mut best_area = f32::INFINITY;
    let mut best_bbox = None;
    for line in basemap_data::BASEMAP_US_STATE_LINES {
        if !bbox_contains(line.bbox, lon, lat) || !basemap_line_contains_lon_lat(line, lon, lat) {
            continue;
        }
        let area = (line.bbox[2] - line.bbox[0]).abs() * (line.bbox[3] - line.bbox[1]).abs();
        if area < best_area {
            best_area = area;
            best_bbox = Some(line.bbox);
        }
    }
    best_bbox.map(us_state_abbr_for_bbox)
}

/// The marker in `points` nearest to `pointer` within the shared 12 px
/// click/hover halo, with its distance — one hit test for the CONUS,
/// international, and community marker sets.
pub(crate) fn nearest_marker_within(
    points: &[(usize, egui::Pos2)],
    pointer: egui::Pos2,
) -> Option<(usize, f32)> {
    points
        .iter()
        .filter_map(|(index, position)| {
            let distance = position.distance(pointer);
            (distance <= 12.0).then_some((*index, distance))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))
}

/// Which marker a click resolves to across the marker families: the
/// nearest in-halo hit wins; exact distance ties keep the declaration
/// order CONUS > international > community > custom poll > RAOB (preserving the
/// historical CONUS-wins-ties behavior; RAOB is an overlay and always
/// loses ties to radar markers). Inputs are [`nearest_marker_within`]
/// results.
pub(crate) fn resolve_marker_click(
    conus: Option<(usize, f32)>,
    intl: Option<(usize, f32)>,
    community: Option<(usize, f32)>,
    custom_poll: Option<(usize, f32)>,
    raob: Option<(usize, f32)>,
) -> Option<(MarkerFamily, usize)> {
    [
        conus.map(|(index, distance)| (MarkerFamily::Conus, index, distance)),
        intl.map(|(index, distance)| (MarkerFamily::Intl, index, distance)),
        community.map(|(index, distance)| (MarkerFamily::Community, index, distance)),
        custom_poll.map(|(index, distance)| (MarkerFamily::CustomPoll, index, distance)),
        raob.map(|(index, distance)| (MarkerFamily::Raob, index, distance)),
    ]
    .into_iter()
    .flatten()
    // Stable: min_by keeps the FIRST of equal elements, i.e. family order.
    .min_by(|left, right| left.2.total_cmp(&right.2))
    .map(|(family, index, _)| (family, index))
}

pub(crate) fn format_site_label(site: &RadarSite) -> String {
    match &site.name {
        Some(name) if !name.is_empty() => format!("{} {}", site.level2_id, name),
        _ => site.level2_id.clone(),
    }
}

pub(crate) fn site_marker_label(site: &RadarSite) -> String {
    match &site.name {
        Some(name) if !name.trim().is_empty() => format!("{} {}", site.level2_id, name.trim()),
        _ if site_is_terminal_radar(site) => format!("{} TDWR", site.level2_id),
        _ => site.level2_id.clone(),
    }
}

pub(crate) fn parse_custom_poll_marker_inputs(
    lat_input: &str,
    lon_input: &str,
) -> std::result::Result<(i64, i64), &'static str> {
    let lat_input = lat_input.trim();
    let lon_input = lon_input.trim();
    if lat_input.is_empty() && lon_input.is_empty() {
        return Ok((CUSTOM_POLL_NO_MARKER_LAT_E6, CUSTOM_POLL_NO_MARKER_LON_E6));
    }
    if lat_input.is_empty() || lon_input.is_empty() {
        return Err("Custom poll link: enter both latitude and longitude, or leave both blank");
    }
    let lat = lat_input
        .parse::<f32>()
        .map_err(|_| "Custom poll link: latitude must be -90 to 90")?;
    if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
        return Err("Custom poll link: latitude must be -90 to 90");
    }
    let lon = lon_input
        .parse::<f32>()
        .map_err(|_| "Custom poll link: longitude must be -180 to 180")?;
    if !lon.is_finite() || !(-180.0..=180.0).contains(&lon) {
        return Err("Custom poll link: longitude must be -180 to 180");
    }
    Ok((
        (lat * 1_000_000.0).round() as i64,
        (lon * 1_000_000.0).round() as i64,
    ))
}

// Map-paint sub-move D (layers): the basemap (tile underlay plus vector
// country / state / county lines and the regional-country overlays), the
// radar reflectivity / velocity raster and its per-layer overlays, and the
// native Italy DPC / Taiwan CWA radar layers, moved VERBATIM out of
// `main.rs`. The entry-point painters are `pub(crate)` because the residual
// paint dispatch (`single_pane_canvas` / `grid_canvas`), the first `impl
// ViewerApp` block, sibling modules (`hazard_ui`), and the `#[cfg(test)]`
// module call them; `ShapeCache` / `RegionalBasemapLayer` are re-exported
// from `main.rs` for the cache fields and the `REGIONAL_BASEMAP_LAYERS`
// table that stay there.
impl ViewerApp {
    pub(crate) fn draw_radar_layer(
        &self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        rect: egui::Rect,
    ) {
        let Some(volume) = self.volume.as_ref() else {
            return;
        };
        let Some((latitude_deg, longitude_deg)) = self.radar_location() else {
            return;
        };
        if let Some(texture) = &self.texture {
            let image_rect = self
                .texture_key
                .as_ref()
                .map(|key| self.radar_texture_rect(ctx, rect, latitude_deg, longitude_deg, key))
                .unwrap_or(rect);

            let baked = pane_or_key_rotation_rad(&self.texture_key);
            paint_rotated_image(
                painter,
                texture.id(),
                image_rect,
                self.lon_lat_to_screen(rect, longitude_deg, latitude_deg),
                self.aeqd_north_angle(rect, latitude_deg, longitude_deg) - baked,
                egui::Color32::from_white_alpha((self.radar_opacity * 255.0) as u8),
            );
        }
        let rings = self.style_registry.range_rings();
        if self.style_registry.radar_age().ring_enabled {
            self.draw_range_ring(
                painter,
                rect,
                latitude_deg,
                longitude_deg,
                self.radar_range_km,
                egui::Stroke::new(
                    rings.primary_width,
                    self.data_edge_ring_color(volume.volume_time.with_timezone(&Utc), 230),
                ),
            );
        }
    }

    /// Data-edge ring color per the range-ring style: scan-age gradient
    /// (default) or a fixed color, both at the caller's alpha.
    pub(crate) fn data_edge_ring_color(
        &self,
        volume_time_utc: DateTime<Utc>,
        alpha: u8,
    ) -> egui::Color32 {
        match self.style_registry.range_rings().color_mode {
            styles::RingColorMode::Age => freshness_ring_color(
                volume_time_utc,
                Utc::now(),
                alpha,
                self.style_registry.radar_age(),
            ),
            styles::RingColorMode::Fixed { color } => {
                egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], alpha)
            }
        }
    }

    pub(crate) fn draw_radar_overlay_layers(
        &self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        rect: egui::Rect,
    ) {
        for layer in &self.radar_layers {
            if !layer.visible {
                continue;
            }
            let Some(volume) = layer.volume.as_ref() else {
                continue;
            };
            let Some((latitude_deg, longitude_deg)) = layer.radar_location() else {
                continue;
            };
            if let Some(texture) = &layer.texture {
                let image_rect = layer
                    .texture_key
                    .as_ref()
                    .map(|key| self.radar_texture_rect(ctx, rect, latitude_deg, longitude_deg, key))
                    .unwrap_or(rect);
                let baked = pane_or_key_rotation_rad(&layer.texture_key);
                paint_rotated_image(
                    painter,
                    texture.id(),
                    image_rect,
                    self.lon_lat_to_screen(rect, longitude_deg, latitude_deg),
                    self.aeqd_north_angle(rect, latitude_deg, longitude_deg) - baked,
                    egui::Color32::from_white_alpha(layer.opacity),
                );
            }
            if self.style_registry.radar_age().ring_enabled {
                self.draw_range_ring(
                    painter,
                    rect,
                    latitude_deg,
                    longitude_deg,
                    layer.radar_range_km,
                    egui::Stroke::new(
                        self.style_registry.range_rings().overlay_width,
                        self.data_edge_ring_color(
                            volume.volume_time.with_timezone(&Utc),
                            layer.opacity,
                        ),
                    ),
                );
            }
        }
    }

    pub(crate) fn radar_texture_rect(
        &self,
        ctx: &egui::Context,
        rect: egui::Rect,
        radar_lat: f32,
        radar_lon: f32,
        texture_key: &TextureKey,
    ) -> egui::Rect {
        let Some((current, _)) =
            self.viewport_raster_options_for_location(ctx, rect, radar_lat, radar_lon)
        else {
            return rect;
        };
        anchored_radar_texture_rect(rect, ctx.pixels_per_point(), texture_key.viewport, current)
    }

    /// Cache key for view-pure draw geometry: layer tag + cell rect + the
    /// shared geo transform. Bit-exact — any pan/zoom/resize changes it.
    pub(crate) fn view_shape_key(&self, tag: u8, rect: egui::Rect) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        tag.hash(&mut hasher);
        rect.min.x.to_bits().hash(&mut hasher);
        rect.min.y.to_bits().hash(&mut hasher);
        rect.max.x.to_bits().hash(&mut hasher);
        rect.max.y.to_bits().hash(&mut hasher);
        self.map_center_lon.to_bits().hash(&mut hasher);
        self.map_center_lat.to_bits().hash(&mut hasher);
        self.map_scale.to_bits().hash(&mut hasher);
        self.basemap_style.key().hash(&mut hasher);
        settings_basemap_line_brightness(&self.app_settings)
            .to_bits()
            .hash(&mut hasher);
        settings_basemap_line_thickness(&self.app_settings)
            .to_bits()
            .hash(&mut hasher);
        self.app_settings.basemap_lightweight.hash(&mut hasher);
        hasher.finish()
    }

    pub(crate) fn draw_basemap(&self, painter: &egui::Painter, rect: egui::Rect) {
        self.draw_tile_basemap(painter, rect);
        // Polyline reprojection is cached per view key (pure in rect + geo
        // transform); idle repaints reuse the projected shapes.
        let key = self.view_shape_key(0, rect);
        let mut cache = self.basemap_shape_cache.borrow_mut();
        let shapes = cache.get_or_insert_with(key, || self.build_basemap_shapes(rect));
        painter.extend(shapes.iter().cloned());
    }

    /// Raster tile basemap: Web-Mercator tiles drawn as AEQD-warped textured
    /// quads beneath everything else. Missing tiles are queued for the
    /// background fetch pool and simply leave the dark background until they
    /// arrive — the UI thread never blocks.
    fn draw_tile_basemap(&self, painter: &egui::Painter, rect: egui::Rect) {
        let style = self.basemap_style;
        let tile_debug = std::env::var_os("BOWECHO_TILE_DEBUG").is_some();
        if style == tiles::TileStyle::DarkVector {
            if tile_debug {
                eprintln!("TILES: style is DarkVector, skipping");
            }
            return;
        }
        let source = tiles::TileSource::Basemap(style);
        self.draw_tile_source(
            painter,
            rect,
            &source,
            egui::Color32::WHITE,
            style.attribution(),
        );
    }

    pub(crate) fn draw_italy_dpc_layer(&mut self, painter: &egui::Painter, rect: egui::Rect) {
        let Some((frame, layer_opacity)) = self.italy_dpc_layer.as_ref().and_then(|layer| {
            layer
                .visible
                .then(|| {
                    layer
                        .frame
                        .as_ref()
                        .cloned()
                        .map(|frame| (frame, layer.opacity))
                })
                .flatten()
        }) else {
            return;
        };
        let view = self.model_layer_current_view();
        let current = self
            .italy_dpc_texture
            .as_ref()
            .filter(|(_, generation, _)| *generation == frame.generation);
        let needs_render = current
            .map(|(_, _, have)| model_layer_view_needs_rerender(have, &view))
            .unwrap_or(true);
        let defer_render = map_layer_rerender_deferred(painter.ctx());
        if needs_render && !defer_render && self.italy_dpc_render_rx.is_none() {
            let (sender, receiver) = mpsc::channel();
            self.italy_dpc_render_rx = Some(receiver);
            let render_view = view;
            let generation = frame.generation;
            let image_src = Arc::clone(&frame.image);
            let lut = Arc::clone(&frame.lut);
            let (nx, ny, flip) = (frame.nx, frame.ny, frame.flip_rows);
            let center_lat = view.center_lat as f64;
            let center_lon = view.center_lon as f64;
            let km_per_pt = 111.32 / view.map_scale as f64;
            let render_scale = italy_dpc::ITALY_DPC_RENDER_SCALE.clamp(0.25, 1.0) as f64;
            let (w_pts, h_pts) = (rect.width() as f64, rect.height() as f64);
            thread::spawn(move || {
                let render_start = Instant::now();
                let w = (w_pts * render_scale).max(8.0) as usize;
                let h = (h_pts * render_scale).max(8.0) as usize;
                let mut pixels = vec![egui::Color32::TRANSPARENT; w * h];
                for (i, px) in pixels.iter_mut().enumerate() {
                    let x = (i % w) as f64 / render_scale;
                    let y = (i / w) as f64 / render_scale;
                    let east_km = (x - w_pts / 2.0) * km_per_pt;
                    let north_km = (h_pts / 2.0 - y) * km_per_pt;
                    let (lat, lon) = aeqd_inverse_km(center_lat, center_lon, east_km, north_km);
                    let Some(index) = lut.lookup(lat as f32, lon as f32) else {
                        continue;
                    };
                    let (row, col) = (index / nx, index % nx);
                    if row >= ny {
                        continue;
                    }
                    let image_row = if flip { ny - 1 - row } else { row };
                    let color = image_src.pixels[image_row * nx + col];
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
            });
        }
        if let Some((texture, _, rendered)) = &self.italy_dpc_texture {
            if model_layer_view_needs_rerender(rendered, &view) {
                return;
            }
            let opacity = (layer_opacity.clamp(0.0, 1.0) * 255.0) as u8;
            painter.image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::from_white_alpha(opacity),
            );
            painter.text(
                egui::pos2(rect.left() + 6.0, rect.bottom() - 4.0),
                egui::Align2::LEFT_BOTTOM,
                "Radar-DPC CC-BY-SA · raw GeoTIFF",
                egui::FontId::proportional(9.0),
                egui::Color32::from_rgba_unmultiplied(230, 234, 238, 150),
            );
        }
    }

    pub(crate) fn draw_taiwan_cwa_layer(&mut self, painter: &egui::Painter, rect: egui::Rect) {
        let Some((frame, layer_opacity)) = self.taiwan_cwa_layer.as_ref().and_then(|layer| {
            layer
                .visible
                .then(|| {
                    layer
                        .frame
                        .as_ref()
                        .cloned()
                        .map(|frame| (frame, layer.opacity))
                })
                .flatten()
        }) else {
            return;
        };
        let view = self.model_layer_current_view();
        let current = self
            .taiwan_cwa_texture
            .as_ref()
            .filter(|(_, generation, _)| *generation == frame.generation);
        let needs_render = current
            .map(|(_, _, have)| model_layer_view_needs_rerender(have, &view))
            .unwrap_or(true);
        let defer_render = map_layer_rerender_deferred(painter.ctx());
        if needs_render && !defer_render && self.taiwan_cwa_render_rx.is_none() {
            let (sender, receiver) = mpsc::channel();
            self.taiwan_cwa_render_rx = Some(receiver);
            let render_view = view;
            let generation = frame.generation;
            let frame = Arc::clone(&frame);
            let center_lat = view.center_lat as f64;
            let center_lon = view.center_lon as f64;
            let km_per_pt = 111.32 / view.map_scale as f64;
            let (w_pts, h_pts) = (rect.width() as f64, rect.height() as f64);
            let render_scale =
                taiwan_cwa::render_scale_for_viewport(rect.width(), rect.height()) as f64;
            thread::spawn(move || {
                let render_start = Instant::now();
                let w = (w_pts * render_scale).max(8.0) as usize;
                let h = (h_pts * render_scale).max(8.0) as usize;
                let mut pixels = vec![egui::Color32::TRANSPARENT; w * h];
                for (i, px) in pixels.iter_mut().enumerate() {
                    let x = (i % w) as f64 / render_scale;
                    let y = (i / w) as f64 / render_scale;
                    let east_km = (x - w_pts / 2.0) * km_per_pt;
                    let north_km = (h_pts / 2.0 - y) * km_per_pt;
                    let (lat, lon) = aeqd_inverse_km(center_lat, center_lon, east_km, north_km);
                    let color =
                        taiwan_cwa::sample_reflectivity_color(&frame, lat as f32, lon as f32);
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
            });
        }
        if let Some((texture, _, rendered)) = &self.taiwan_cwa_texture {
            if model_layer_view_needs_rerender(rendered, &view) {
                return;
            }
            let opacity = (layer_opacity.clamp(0.0, 1.0) * 255.0) as u8;
            painter.image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::from_white_alpha(opacity),
            );
            painter.text(
                egui::pos2(rect.left() + 6.0, rect.bottom() - 18.0),
                egui::Align2::LEFT_BOTTOM,
                "Taiwan CWA O-A0059-001 · composite REF",
                egui::FontId::proportional(9.0),
                egui::Color32::from_rgba_unmultiplied(230, 234, 238, 150),
            );
        }
    }

    fn draw_tile_source(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        source: &tiles::TileSource,
        tint: egui::Color32,
        attribution: Option<&str>,
    ) {
        let tile_debug = std::env::var_os("BOWECHO_TILE_DEBUG").is_some();
        let pixels_per_point = painter.ctx().pixels_per_point().max(0.5);
        let km_per_px = 111.32 / self.map_scale;
        let view_zoom = tiles::zoom_for_km_per_px(km_per_px, self.map_center_lat, pixels_per_point);
        let zoom = source.stable_source_zoom().unwrap_or(view_zoom);
        let mut bounds = self.visible_geo_bounds(rect);
        if let Some((west, south, east, north)) = source.geo_clip_bounds() {
            let Some(clipped) = bounds.intersect(GeoBounds {
                west,
                south,
                east,
                north,
            }) else {
                return;
            };
            bounds = clipped;
        }
        let (x0, y0) = tiles::tile_coords(bounds.west as f64, bounds.north as f64, zoom);
        let (x1, y1) = tiles::tile_coords(bounds.east as f64, bounds.south as f64, zoom);
        let n = 1u32 << zoom;
        let clamp_tile = |v: f64| (v.floor().max(0.0) as u32).min(n - 1);
        let (tx0, tx1) = (clamp_tile(x0), clamp_tile(x1));
        let (ty0, ty1) = (clamp_tile(y0), clamp_tile(y1));
        if tile_debug {
            eprintln!(
                "TILES: view zoom {view_zoom} source zoom {zoom} x {tx0}..{tx1} y {ty0}..{ty1} bounds W{:.2} E{:.2} S{:.2} N{:.2}",
                bounds.west, bounds.east, bounds.south, bounds.north
            );
        }
        // Hard cap so degenerate bounds never flood the queue.
        if (tx1.saturating_sub(tx0) + 1) as u64 * (ty1.saturating_sub(ty0) + 1) as u64 > 120 {
            if tile_debug {
                eprintln!("TILES: over tile cap, skipping");
            }
            return;
        }
        // Great-circle central angle, for dropping tiles near the AEQD
        // antipode where the projection smears them across the rim.
        fn central_angle_deg(lat_a: f32, lon_a: f32, lat_b: f32, lon_b: f32) -> f32 {
            let (la, lb) = (lat_a.to_radians(), lat_b.to_radians());
            let dlon = (lon_b - lon_a).to_radians();
            (la.sin() * lb.sin() + la.cos() * lb.cos() * dlon.cos())
                .clamp(-1.0, 1.0)
                .acos()
                .to_degrees()
        }
        let mut layer = self.tile_layer.borrow_mut();
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let tile = tiles::TileId { zoom, x: tx, y: ty };
                // Far-hemisphere guard: a tile near the antipode projects
                // to an enormous smeared ring — the vector basemap keeps
                // drawing there, raster just bows out (field screenshot:
                // planet-zoom slabs and giant Antarctica).
                let (center_lon, center_lat) =
                    tiles::tile_corner_lon_lat(tx as f64 + 0.5, ty as f64 + 0.5, zoom);
                if central_angle_deg(
                    self.map_center_lat,
                    self.map_center_lon,
                    center_lat as f32,
                    center_lon as f32,
                ) > 140.0
                {
                    continue;
                }
                let Some(texture_id) = layer
                    .texture_source(source, tile)
                    .map(|texture| texture.id())
                else {
                    layer.request_source(source.clone(), tile);
                    continue;
                };
                // Coarse 4-corner probe sizes the tile on screen and picks
                // the mesh density: more cells the bigger the tile, so
                // AEQD curvature bends the texture instead of shearing a
                // single quad (same trick as the FARM/WoFS 8x8 drapes).
                let probe = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)].map(|(fx, fy)| {
                    let (lon, lat) =
                        tiles::tile_corner_lon_lat(tx as f64 + fx, ty as f64 + fy, zoom);
                    self.lon_lat_to_screen(rect, lon as f32, lat as f32)
                });
                let span_px = {
                    let probe_bounds = egui::Rect::from_points(&probe);
                    probe_bounds.width().max(probe_bounds.height())
                };
                let grid: u32 = if span_px > 512.0 {
                    16
                } else if span_px > 160.0 {
                    8
                } else {
                    4
                };
                let mut vertices = Vec::with_capacity(((grid + 1) * (grid + 1)) as usize);
                let mut mesh_bounds: Option<egui::Rect> = None;
                for gy in 0..=grid {
                    for gx in 0..=grid {
                        let (fx, fy) = (
                            f64::from(gx) / f64::from(grid),
                            f64::from(gy) / f64::from(grid),
                        );
                        let (lon, lat) =
                            tiles::tile_corner_lon_lat(tx as f64 + fx, ty as f64 + fy, zoom);
                        let pos = self.lon_lat_to_screen(rect, lon as f32, lat as f32);
                        mesh_bounds = Some(match mesh_bounds {
                            Some(bounds) => bounds.union(egui::Rect::from_min_max(pos, pos)),
                            None => egui::Rect::from_min_max(pos, pos),
                        });
                        vertices.push((pos, egui::pos2(fx as f32, fy as f32)));
                    }
                }
                if vertices
                    .iter()
                    .any(|(pos, _)| !pos.x.is_finite() || !pos.y.is_finite())
                {
                    continue; // beyond the projection's validity
                }
                // Cull on the FULL mesh bounds — curved edges bow outside
                // the corner hull, so 4-corner culling drops live tiles.
                let Some(mesh_bounds) = mesh_bounds else {
                    continue;
                };
                if !rect.intersects(mesh_bounds) {
                    continue;
                }
                let mut mesh = egui::epaint::Mesh::with_texture(texture_id);
                for (pos, uv) in &vertices {
                    mesh.vertices.push(egui::epaint::Vertex {
                        pos: *pos,
                        uv: *uv,
                        color: tint,
                    });
                }
                let stride = grid + 1;
                for gy in 0..grid {
                    for gx in 0..grid {
                        let i = gy * stride + gx;
                        mesh.indices.extend_from_slice(&[
                            i,
                            i + 1,
                            i + stride,
                            i + 1,
                            i + stride + 1,
                            i + stride,
                        ]);
                    }
                }
                painter.add(egui::Shape::mesh(mesh));
            }
        }
        if let Some(attribution) = attribution {
            painter.text(
                egui::pos2(rect.left() + 6.0, rect.bottom() - 4.0),
                egui::Align2::LEFT_BOTTOM,
                attribution,
                egui::FontId::proportional(9.0),
                egui::Color32::from_rgba_unmultiplied(230, 234, 238, 150),
            );
        }
    }

    fn build_basemap_shapes(&self, rect: egui::Rect) -> Vec<egui::Shape> {
        let mut sink = Vec::new();
        let bounds = self.visible_geo_bounds(rect).expand(0.25);
        let us_detail_visible = us_detail_visible(bounds);
        let lightweight = self.app_settings.basemap_lightweight;
        let strokes = basemap_underlay_strokes(
            self.basemap_style,
            settings_basemap_line_brightness(&self.app_settings),
            settings_basemap_line_thickness(&self.app_settings),
        );
        self.collect_world_country_line_shapes(
            rect,
            bounds,
            strokes.world,
            us_detail_visible,
            &mut sink,
        );

        if !lightweight && us_detail_visible && self.map_scale >= 38.0 {
            self.collect_basemap_line_shapes(
                rect,
                bounds,
                basemap_data::BASEMAP_US_COUNTY_LINES,
                strokes.county,
                &mut sink,
            );
        }
        if us_detail_visible {
            self.collect_basemap_line_shapes(
                rect,
                bounds,
                basemap_data::BASEMAP_US_STATE_LINES,
                strokes.state,
                &mut sink,
            );
        }

        if !lightweight && self.map_scale >= 36.0 {
            for layer in REGIONAL_BASEMAP_LAYERS {
                if bounds.intersects_bbox(layer.bounds) {
                    self.collect_basemap_line_shapes(
                        rect,
                        bounds,
                        layer.admin_lines,
                        strokes.regional,
                        &mut sink,
                    );
                }
            }
        }
        sink
    }

    pub(crate) fn draw_basemap_overlay(&self, painter: &egui::Painter, rect: egui::Rect) {
        let bounds = self.visible_geo_bounds(rect).expand(0.15);
        // Lines are view-pure and cached; labels (font layout + collision
        // budgets) stay live each frame.
        let key = self.view_shape_key(1, rect);
        let mut cache = self.basemap_shape_cache.borrow_mut();
        let shapes = cache.get_or_insert_with(key, || self.build_basemap_overlay_shapes(rect));
        painter.extend(shapes.iter().cloned());
        drop(cache);

        let mut occupied = Vec::with_capacity(128);
        self.draw_world_place_labels(painter, rect, bounds, &mut occupied);
        self.draw_regional_place_labels(painter, rect, bounds, &mut occupied);
        self.draw_admin_labels(painter, rect, bounds, &mut occupied);
    }

    pub(crate) fn build_basemap_overlay_shapes(&self, rect: egui::Rect) -> Vec<egui::Shape> {
        let mut sink = Vec::new();
        let bounds = self.visible_geo_bounds(rect).expand(0.15);
        let us_detail_visible = us_detail_visible(bounds);
        let lightweight = self.app_settings.basemap_lightweight;
        let strokes = basemap_overlay_strokes(
            self.basemap_style,
            settings_basemap_line_brightness(&self.app_settings),
            settings_basemap_line_thickness(&self.app_settings),
        );
        if self.map_scale >= 18.0 {
            self.collect_world_country_line_shapes(
                rect,
                bounds,
                strokes.world,
                us_detail_visible,
                &mut sink,
            );
        }

        if !lightweight && us_detail_visible && self.map_scale >= 76.0 {
            self.collect_basemap_line_shapes(
                rect,
                bounds,
                basemap_data::BASEMAP_US_COUNTY_LINES,
                strokes.county,
                &mut sink,
            );
        }
        if us_detail_visible {
            self.collect_basemap_line_shapes(
                rect,
                bounds,
                basemap_data::BASEMAP_US_STATE_LINES,
                strokes.state,
                &mut sink,
            );
        }

        if !lightweight && self.map_scale >= 74.0 {
            for layer in REGIONAL_BASEMAP_LAYERS {
                if bounds.intersects_bbox(layer.bounds) {
                    self.collect_basemap_line_shapes(
                        rect,
                        bounds,
                        layer.admin_lines,
                        strokes.regional,
                        &mut sink,
                    );
                }
            }
        }
        sink
    }

    fn collect_world_country_line_shapes(
        &self,
        rect: egui::Rect,
        bounds: GeoBounds,
        stroke: egui::Stroke,
        us_detail_visible: bool,
        sink: &mut Vec<egui::Shape>,
    ) {
        for line in basemap_data::BASEMAP_WORLD_COUNTRY_LINES {
            if us_detail_visible && world_country_line_duplicates_us_detail(line) {
                continue;
            }
            if bounds.intersects_bbox(line.bbox)
                && let Some(shape) = self.geo_line_shape(rect, line.points, stroke)
            {
                sink.push(shape);
            }
        }
    }

    fn collect_basemap_line_shapes(
        &self,
        rect: egui::Rect,
        bounds: GeoBounds,
        lines: &[basemap_data::BasemapLine],
        stroke: egui::Stroke,
        sink: &mut Vec<egui::Shape>,
    ) {
        for line in lines {
            if bounds.intersects_bbox(line.bbox)
                && let Some(shape) = self.geo_line_shape(rect, line.points, stroke)
            {
                sink.push(shape);
            }
        }
    }

    fn geo_line_shape(
        &self,
        rect: egui::Rect,
        coordinates: &[(f32, f32)],
        stroke: egui::Stroke,
    ) -> Option<egui::Shape> {
        if coordinates.len() < 2 {
            return None;
        }
        let simplify_px_sq = basemap_line_simplification_px(self.map_scale).powi(2);
        let mut points = Vec::with_capacity(coordinates.len());
        for (index, (longitude_deg, latitude_deg)) in coordinates.iter().enumerate() {
            let point = self.lon_lat_to_screen(rect, *longitude_deg, *latitude_deg);
            let is_endpoint = index == 0 || index + 1 == coordinates.len();
            if !is_endpoint
                && simplify_px_sq > 0.0
                && points
                    .last()
                    .is_some_and(|last: &egui::Pos2| last.distance_sq(point) < simplify_px_sq)
            {
                continue;
            }
            points.push(point);
        }
        (points.len() >= 2).then(|| egui::Shape::line(points, stroke))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RegionalBasemapLayer {
    pub(crate) bounds: [f32; 4],
    pub(crate) admin_lines: &'static [basemap_data::BasemapLine],
    pub(crate) admin_labels: &'static [basemap_data::BasemapLabel],
    pub(crate) place_labels: &'static [basemap_data::BasemapLabel],
}

#[derive(Clone, Copy)]
pub(crate) struct BasemapLineStrokes {
    pub(crate) world: egui::Stroke,
    pub(crate) county: egui::Stroke,
    pub(crate) state: egui::Stroke,
    pub(crate) regional: egui::Stroke,
}

fn settings_basemap_line_brightness(settings: &settings::AppSettings) -> f32 {
    settings.basemap_line_brightness_percent.clamp(20, 200) as f32 / 100.0
}

fn settings_basemap_line_thickness(settings: &settings::AppSettings) -> f32 {
    settings.basemap_line_thickness_percent.clamp(25, 250) as f32 / 100.0
}

fn basemap_line_brightness(value: f32) -> f32 {
    value.clamp(0.2, 2.0)
}

fn basemap_line_thickness(value: f32) -> f32 {
    value.clamp(0.25, 2.5)
}

fn tune_basemap_line_color(color: egui::Color32, brightness: f32) -> egui::Color32 {
    let brightness = basemap_line_brightness(brightness);
    let scale = |value: u8| ((value as f32 * brightness).round()).clamp(0.0, 255.0) as u8;
    egui::Color32::from_rgba_unmultiplied(
        scale(color.r()),
        scale(color.g()),
        scale(color.b()),
        scale(color.a()),
    )
}

fn tune_basemap_stroke(stroke: egui::Stroke, brightness: f32, thickness: f32) -> egui::Stroke {
    egui::Stroke::new(
        stroke.width * basemap_line_thickness(thickness),
        tune_basemap_line_color(stroke.color, brightness),
    )
}

fn tune_basemap_strokes(
    strokes: BasemapLineStrokes,
    brightness: f32,
    thickness: f32,
) -> BasemapLineStrokes {
    BasemapLineStrokes {
        world: tune_basemap_stroke(strokes.world, brightness, thickness),
        county: tune_basemap_stroke(strokes.county, brightness, thickness),
        state: tune_basemap_stroke(strokes.state, brightness, thickness),
        regional: tune_basemap_stroke(strokes.regional, brightness, thickness),
    }
}

pub(crate) fn basemap_underlay_strokes(
    style: tiles::TileStyle,
    brightness: f32,
    thickness: f32,
) -> BasemapLineStrokes {
    let strokes = if style == tiles::TileStyle::Satellite {
        BasemapLineStrokes {
            world: egui::Stroke::new(
                0.9,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 176),
            ),
            county: egui::Stroke::new(
                0.65,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 102),
            ),
            state: egui::Stroke::new(
                1.05,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 166),
            ),
            regional: egui::Stroke::new(
                0.85,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 146),
            ),
        }
    } else {
        BasemapLineStrokes {
            world: egui::Stroke::new(0.75, egui::Color32::from_rgb(31, 45, 57)),
            county: egui::Stroke::new(0.65, egui::Color32::from_rgb(24, 35, 46)),
            state: egui::Stroke::new(1.05, egui::Color32::from_rgb(41, 58, 73)),
            regional: egui::Stroke::new(0.85, egui::Color32::from_rgb(36, 52, 65)),
        }
    };
    tune_basemap_strokes(strokes, brightness, thickness)
}

pub(crate) fn basemap_overlay_strokes(
    style: tiles::TileStyle,
    brightness: f32,
    thickness: f32,
) -> BasemapLineStrokes {
    let strokes = if style == tiles::TileStyle::Satellite {
        BasemapLineStrokes {
            world: egui::Stroke::new(
                0.85,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 118),
            ),
            county: egui::Stroke::new(
                0.55,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 108),
            ),
            state: egui::Stroke::new(
                1.0,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 148),
            ),
            regional: egui::Stroke::new(
                0.75,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 128),
            ),
        }
    } else {
        BasemapLineStrokes {
            world: egui::Stroke::new(
                0.85,
                egui::Color32::from_rgba_unmultiplied(102, 126, 145, 84),
            ),
            county: egui::Stroke::new(
                0.55,
                egui::Color32::from_rgba_unmultiplied(92, 112, 128, 92),
            ),
            state: egui::Stroke::new(
                1.0,
                egui::Color32::from_rgba_unmultiplied(126, 150, 170, 116),
            ),
            regional: egui::Stroke::new(
                0.75,
                egui::Color32::from_rgba_unmultiplied(112, 136, 154, 96),
            ),
        }
    };
    tune_basemap_strokes(strokes, brightness, thickness)
}

fn basemap_line_simplification_px(map_scale: f32) -> f32 {
    if map_scale < 24.0 {
        0.75
    } else if map_scale < 96.0 {
        0.45
    } else {
        0.0
    }
}

/// Tiny keyed cache for draw geometry that is pure in the view state
/// (projected basemap polylines, tessellated hazard polygons). Idle repaints —
/// texture arrivals, hovers, animations — reuse entries instead of
/// reprojecting / re-ear-clipping every frame; any pan/zoom/content change
/// alters the key and falls through to a rebuild. Keys include the cell rect,
/// so multi-pane grids cache one entry per pane. LRU, capacity-capped.
pub(crate) struct ShapeCache<V> {
    entries: Vec<(u64, V)>,
    capacity: usize,
}

impl<V> ShapeCache<V> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    pub(crate) fn get_or_insert_with(&mut self, key: u64, build: impl FnOnce() -> V) -> &V {
        if let Some(position) = self.entries.iter().position(|(k, _)| *k == key) {
            let entry = self.entries.remove(position);
            self.entries.push(entry);
        } else {
            if self.entries.len() >= self.capacity {
                self.entries.remove(0);
            }
            self.entries.push((key, build()));
        }
        &self.entries.last().expect("just pushed").1
    }
}

#[cfg(test)]
mod loupe_native_gates_tests {
    use super::*;
    use color_tables::{ColorTableFamily, ColorTableSet};
    use radar_core::{ElevationCut, GateRange, MomentGrid, MomentStorage, MomentType, Radial};

    fn gate_range(gate_count: usize) -> GateRange {
        GateRange {
            first_gate_m: 0,
            gate_spacing_m: 250,
            gate_count,
        }
    }

    fn test_radial(azimuth_deg: f32, range: GateRange) -> Radial {
        Radial {
            azimuth_deg,
            elevation_deg: 0.5,
            time_offset_ms: 0,
            gate_range: range,
            nyquist_velocity_mps: Some(32.0),
            radial_status: None,
        }
    }

    /// A cut whose reflectivity grid has `radial_count` evenly spaced radials
    /// (0..360) and every gate set to `raw`. scale=2, offset=66, nodata=0,
    /// range_folded=1 — the classic Level-II REF packing.
    fn uniform_ref(radial_count: usize, gate_count: usize, raw: u8) -> (ElevationCut, MomentGrid) {
        let range = gate_range(gate_count);
        let mut cut = ElevationCut::new(0.5, Some(1));
        let step = 360.0 / radial_count as f32;
        for row in 0..radial_count {
            cut.radials
                .push(test_radial(row as f32 * step, range.clone()));
        }
        let mut grid =
            MomentGrid::new_u8(MomentType::Reflectivity, range, 2.0, 66.0, Some(0), Some(1));
        grid.radial_indices = (0..radial_count).collect();
        grid.storage = MomentStorage::U8(vec![raw; radial_count * gate_count]);
        (cut, grid)
    }

    fn ref_grid_only(gate_count: usize, raws: &[u8]) -> MomentGrid {
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range(gate_count),
            2.0,
            66.0,
            Some(0),
            Some(1),
        );
        grid.radial_indices = vec![0];
        grid.storage = MomentStorage::U8(raws.to_vec());
        grid
    }

    #[test]
    fn gate_color_nodata_is_transparent() {
        // raw == nodata (0) -> no gate drawn.
        let grid = ref_grid_only(3, &[0, 120, 1]);
        let table = ColorTableSet::default()
            .for_family(ColorTableFamily::Reflectivity)
            .clone();
        let sampler = table.sampler();
        assert_eq!(loupe_gate_color(&grid, &sampler, 0, 0), None);
    }

    #[test]
    fn gate_color_range_folded_uses_rf_color() {
        // raw == range_folded (1) -> the table's dedicated RF color, exactly as
        // the raster paints it (not transparent, not a scaled sample).
        let grid = ref_grid_only(3, &[0, 120, 1]);
        let table = ColorTableSet::default()
            .for_family(ColorTableFamily::Velocity)
            .clone();
        let sampler = table.sampler();
        let rf = table.range_folded_color();
        let expected = (rf[3] != 0).then_some(rf);
        assert_eq!(loupe_gate_color(&grid, &sampler, 0, 2), expected);
    }

    #[test]
    fn gate_color_scaled_matches_the_table() {
        // A normal gate colors via the SAME table.color_for_value the map uses.
        let grid = ref_grid_only(3, &[0, 120, 1]);
        let table = ColorTableSet::default()
            .for_family(ColorTableFamily::Reflectivity)
            .clone();
        let sampler = table.sampler();
        let value = (120.0 - 66.0) / 2.0; // 27 dBZ
        let expected = table.color_for_value(value);
        assert_eq!(
            loupe_gate_color(&grid, &sampler, 0, 1),
            (expected[3] != 0).then_some(expected)
        );
    }

    #[test]
    fn gate_color_f32_nan_is_transparent() {
        let mut grid = MomentGrid::new_u16(
            MomentType::Reflectivity,
            gate_range(2),
            1.0,
            0.0,
            None,
            None,
        );
        grid.radial_indices = vec![0];
        grid.storage = MomentStorage::F32(vec![f32::NAN, 25.0]);
        let table = ColorTableSet::default()
            .for_family(ColorTableFamily::Reflectivity)
            .clone();
        let sampler = table.sampler();
        assert_eq!(loupe_gate_color(&grid, &sampler, 0, 0), None);
        assert_eq!(
            loupe_gate_color(&grid, &sampler, 0, 1),
            Some(table.color_for_value(25.0))
        );
    }

    #[test]
    fn geom_range_az_inverts_the_readout_mapping() {
        // radar at screen origin, no AEQD rotation, 1 km/px. Due-east screen
        // offset -> azimuth 90, due-north (screen y up) -> azimuth 0.
        let geom = LoupeGateGeom {
            radar_pos: egui::pos2(0.0, 0.0),
            angle: 0.0,
            km_per_px: 1.0,
            max_range_km: 500.0,
        };
        let (range_e, az_e) = geom.range_az(egui::pos2(100.0, 0.0)).expect("in range");
        assert!((range_e - 100.0).abs() < 1e-3);
        assert!((az_e - 90.0).abs() < 1e-3);
        let (range_n, az_n) = geom.range_az(egui::pos2(0.0, -100.0)).expect("in range");
        assert!((range_n - 100.0).abs() < 1e-3);
        assert!(az_n.abs() < 1e-3 || (az_n - 360.0).abs() < 1e-3);
        // Beyond the grid range -> no gate.
        assert!(geom.range_az(egui::pos2(0.0, 600.0)).is_none());
    }

    #[test]
    fn az_lut_resolves_the_nearest_radial_and_gaps() {
        // 360 radials at 1 deg: threshold 0.55 deg. Exact and near hits resolve;
        // a 2-deg-spaced cut leaves a gap that returns None mid-way.
        let (dense_cut, dense_grid) = uniform_ref(360, 4, 120);
        let lut = LoupeAzLut::build(&dense_cut, &dense_grid);
        assert_eq!(lut.row(90.0), Some(90));
        assert_eq!(lut.row(90.2), Some(90));
        assert_eq!(lut.row(359.9), Some(0));

        let (sparse_cut, sparse_grid) = uniform_ref(180, 4, 120); // 2 deg spacing
        let sparse = LoupeAzLut::build(&sparse_cut, &sparse_grid);
        assert_eq!(sparse.row(0.2), Some(0)); // within 0.8 deg threshold
        assert_eq!(sparse.row(1.0), None); // 1 deg from either radial -> gap
    }

    #[test]
    fn build_image_colors_the_focus_gate_and_clears_outside_the_disk() {
        let (cut, grid) = uniform_ref(360, 600, 120);
        let table = ColorTableSet::default()
            .for_family(ColorTableFamily::Reflectivity)
            .clone();
        let sampler = table.sampler();
        let az_lut = LoupeAzLut::build(&cut, &grid);
        let geom = LoupeGateGeom {
            radar_pos: egui::pos2(0.0, 0.0),
            angle: 0.0,
            km_per_px: 1.0,
            max_range_km: 150.0,
        };
        let radius = 70.0_f32;
        let magnify = 6.0_f32;
        let center = egui::pos2(300.0, 300.0);
        let focus = egui::pos2(100.0, 0.0); // due east of radar, range 100 km
        let dim = (2.0 * radius).ceil() as usize;
        let image = build_radar_loupe_image(
            dim, center, focus, radius, magnify, &geom, &az_lut, &grid, &sampler,
        );
        assert_eq!(image.size, [dim, dim]);

        let expected = table.color_for_value((120.0 - 66.0) / 2.0);
        let expected = egui::Color32::from_rgba_unmultiplied(
            expected[0],
            expected[1],
            expected[2],
            expected[3],
        );
        // Center texel samples the focus gate exactly.
        let mid = dim / 2;
        assert_eq!(image.pixels[mid * dim + mid], expected);
        // A corner is outside the inscribed disk -> transparent.
        assert_eq!(image.pixels[0], egui::Color32::TRANSPARENT);
        // The magnification is baked in: a rim texel still lands on a valid,
        // colored gate (the loupe shows a real neighborhood, not one gate).
        let rim = loupe_sample_screen(
            focus,
            center,
            center + egui::vec2(radius - 1.0, 0.0),
            magnify,
        );
        assert!(geom.range_az(rim).is_some());
        let opaque = image.pixels.iter().filter(|p| p.a() != 0).count();
        assert!(
            opaque > 100,
            "expected a filled disk, saw {opaque} opaque texels"
        );
    }
}

#[cfg(test)]
mod loupe_polish_tests {
    use super::*;

    // ---- Item 3: scroll routing decision + magnification clamp ----

    #[test]
    fn loupe_owns_scroll_matches_visibility_gate() {
        // Card off -> never (loupe only draws inside the card).
        assert!(!loupe_owns_scroll(false, true, true, false));
        // Card on + toggle on -> owns scroll even without Shift.
        assert!(loupe_owns_scroll(true, true, false, false));
        // Card on + Shift held -> owns scroll (transient loupe).
        assert!(loupe_owns_scroll(true, false, true, false));
        // Card on but neither toggle nor Shift -> map keeps the wheel.
        assert!(!loupe_owns_scroll(true, false, false, false));
        // Plot-domain box owns the surface -> loupe never intercepts.
        assert!(!loupe_owns_scroll(true, true, true, true));
    }

    #[test]
    fn magnify_scrolls_within_clamp() {
        // The default sits inside the clamp: a no-op scroll returns it unchanged
        // (would be pulled to a bound if it were out of range).
        assert!(
            (next_loupe_magnify(LOUPE_MAGNIFY_DEFAULT, 0.0) - LOUPE_MAGNIFY_DEFAULT).abs() < 1e-6
        );
        // Wheel up magnifies more, wheel down less.
        assert!(next_loupe_magnify(6.0, 50.0) > 6.0);
        assert!(next_loupe_magnify(6.0, -50.0) < 6.0);
        // Clamped at both ends no matter how hard you scroll.
        assert!((next_loupe_magnify(19.0, 100000.0) - LOUPE_MAGNIFY_MAX).abs() < 1e-4);
        assert!((next_loupe_magnify(2.5, -100000.0) - LOUPE_MAGNIFY_MIN).abs() < 1e-4);
        // Zero scroll is a no-op.
        assert!((next_loupe_magnify(7.5, 0.0) - 7.5).abs() < 1e-6);
        // A single notch is a modest, monotonic step both ways.
        let up = next_loupe_magnify(6.0, 50.0);
        let back = next_loupe_magnify(up, -50.0);
        assert!((back - 6.0).abs() < 1e-3);
    }

    // ---- Item 1: inspector card placement never overlaps the loupe disk ----

    fn pane() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 800.0))
    }

    fn card_size() -> egui::Vec2 {
        egui::vec2(160.0, 150.0)
    }

    /// The loupe disk geometry the way `cursor_loupe_disk` computes it, so the
    /// placement test uses the real relationship between cursor and disk.
    fn disk_for(anchor: egui::Pos2, rect: egui::Rect) -> (egui::Pos2, f32) {
        let radius = LOUPE_RADIUS;
        let margin = radius + 6.0;
        let mut center = egui::pos2(anchor.x, anchor.y - (radius + 34.0));
        if center.y - radius < rect.top() + 4.0 {
            center.y = anchor.y + (radius + 34.0);
        }
        center.x = center.x.clamp(rect.left() + margin, rect.right() - margin);
        center.y = center.y.clamp(rect.top() + margin, rect.bottom() - margin);
        (center, radius)
    }

    #[test]
    fn default_placement_kept_when_it_already_clears_the_disk() {
        // Cursor mid-pane: loupe floats ABOVE, card default goes BELOW-right —
        // already clear, so the placement is byte-identical to today.
        let rect = pane();
        let anchor = egui::pos2(500.0, 400.0);
        let (dc, dr) = disk_for(anchor, rect);
        let default = anchor + egui::vec2(16.0, 14.0);
        let placed =
            place_inspector_card_clear_of_loupe(default, anchor, card_size(), rect, dc, dr);
        assert_eq!(placed, default);
    }

    #[test]
    fn placement_clears_disk_when_card_would_flip_onto_the_loupe() {
        // Cursor near the BOTTOM: the card's normal bottom-flip sends it UP,
        // onto the loupe disk. Re-placement must clear the disk and stay on
        // screen.
        let rect = pane();
        let anchor = egui::pos2(500.0, 780.0);
        let (dc, dr) = disk_for(anchor, rect);
        // Reproduce the loupe-unaware default (down-right, flipped up because it
        // would run off the bottom).
        let size = card_size();
        let mut default = anchor + egui::vec2(16.0, 14.0);
        if default.y + size.y > rect.bottom() - 4.0 {
            default.y = anchor.y - 14.0 - size.y;
        }
        let placed = place_inspector_card_clear_of_loupe(default, anchor, size, rect, dc, dr);
        let card = egui::Rect::from_min_size(placed, size);
        assert!(
            !card_overlaps_loupe(card, dc, dr),
            "card {card:?} still overlaps loupe at {dc:?} r={dr}"
        );
        assert!(rect.shrink(4.0).contains(card.left_top()));
        assert!(rect.shrink(4.0).contains(card.right_bottom()));
    }

    #[test]
    fn placement_clears_disk_when_loupe_flips_below_cursor() {
        // Cursor near the TOP: the loupe disk flips BELOW the cursor, so the
        // card's default down-right placement lands on it. Re-placement clears.
        let rect = pane();
        let anchor = egui::pos2(500.0, 20.0);
        let (dc, dr) = disk_for(anchor, rect);
        assert!(dc.y > anchor.y, "loupe should be below the cursor here");
        let default = anchor + egui::vec2(16.0, 14.0);
        let placed =
            place_inspector_card_clear_of_loupe(default, anchor, card_size(), rect, dc, dr);
        let card = egui::Rect::from_min_size(placed, card_size());
        assert!(!card_overlaps_loupe(card, dc, dr));
        assert!(rect.shrink(4.0).contains(card.left_top()));
        assert!(rect.shrink(4.0).contains(card.right_bottom()));
    }

    #[test]
    fn placement_stays_on_screen_in_a_corner() {
        // Cursor jammed in the top-left corner: still on-screen and (where the
        // pane allows) off the disk.
        let rect = pane();
        let anchor = egui::pos2(6.0, 6.0);
        let (dc, dr) = disk_for(anchor, rect);
        let default = anchor + egui::vec2(16.0, 14.0);
        let placed =
            place_inspector_card_clear_of_loupe(default, anchor, card_size(), rect, dc, dr);
        let card = egui::Rect::from_min_size(placed, card_size());
        assert!(rect.shrink(4.0).contains(card.left_top()));
        assert!(rect.shrink(4.0).contains(card.right_bottom()));
        assert!(!card_overlaps_loupe(card, dc, dr));
    }

    #[test]
    fn clearance_zero_inside_positive_outside() {
        let card = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 10.0));
        assert!(rect_center_clearance(card, egui::pos2(5.0, 5.0)).abs() < 1e-6);
        assert!((rect_center_clearance(card, egui::pos2(13.0, 5.0)) - 3.0).abs() < 1e-6);
    }
}
