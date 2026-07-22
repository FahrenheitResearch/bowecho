//! Theme-aware vertical wind profile presentation.
//!
//! The science and worker lifecycle deliberately live outside this module.
//! [`VwpPanelState`] accepts an already-computed [`render2d::VwpProfile`] (or
//! a small, decoder-neutral Product 48 display adapter), paints it, and emits
//! actions for the owning `ViewerApp` to service.  Keeping file dialogs and
//! background work out of the view makes the same body usable in a dock tile
//! and a floating window.

// The Product 48 adapter and CSV-only fields are exercised by every supported
// desktop build. Keep dead-code tolerance only for non-desktop targets.
#![cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]

use std::fmt::Write as _;

use chrono::{DateTime, SecondsFormat, Utc};
use eframe::egui;
use render2d::{VwpLevelOutcome, VwpProfile, VwpQuality, VwpRejectionReason};

const WIDE_LAYOUT_MIN_WIDTH: f32 = 720.0;
const MIN_CHART_HEIGHT: f32 = 300.0;
const MAX_CHART_HEIGHT: f32 = 620.0;
const NARROW_CHART_HEIGHT: f32 = 340.0;
const MPS_TO_KT: f32 = 1.943_844_4;
const NM_TO_M: f32 = 1_852.0;

const fn native_file_dialogs_available() -> bool {
    cfg!(any(windows, target_os = "macos", target_os = "linux"))
}

const GOOD_COLOR: egui::Color32 = egui::Color32::from_rgb(58, 194, 126);
const HODO_LOW_COLOR: egui::Color32 = egui::Color32::from_rgb(31, 186, 218);
const HODO_HIGH_COLOR: egui::Color32 = egui::Color32::from_rgb(230, 213, 54);

/// Button requests emitted by [`VwpPanelState::ui`].
///
/// The owner performs the actual work so this module never opens a native
/// dialog or blocks the egui frame.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct VwpPanelAction {
    pub(crate) recompute_requested: bool,
    pub(crate) open_product48_requested: bool,
    pub(crate) save_csv_requested: bool,
}

/// The operator-selected storm-motion vector used by SRV and related tools.
/// Keeping it separate from the retrieved environmental profile prevents the
/// VWP from presenting a user setting as a measured wind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VwpMotionVector {
    pub(crate) direction_deg: f32,
    pub(crate) speed_kt: f32,
}

/// Decoder-neutral quality for an imported NEXRAD Level III Product 48 wind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Product48DisplayQuality {
    Good,
    Marginal,
}

/// One imported Product 48 level after its text/symbology block is decoded.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Product48DisplayLevel {
    pub(crate) height_m_agl: f32,
    pub(crate) height_m_msl: Option<f32>,
    pub(crate) outcome: Product48DisplayOutcome,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Product48DisplayOutcome {
    Retrieved {
        direction_deg: f32,
        speed_mps: f32,
        rms_mps: Option<f32>,
        /// Product 48's tabular divergence value in its native product
        /// units. Symbology-only profiles do not carry it.
        divergence: Option<f32>,
        slant_range_nm: Option<f32>,
        elevation_deg: Option<f32>,
        quality: Product48DisplayQuality,
    },
    // Product 48 imports currently expose only decoded winds, but the display
    // model retains rejected rows for parser variants and deterministic tests.
    #[allow(dead_code)]
    Rejected { reason: String },
}

/// Adaptable-parameter metadata carried by Product 48's tabular block.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Product48DisplayMetadata {
    pub(crate) rms_threshold_kts: Option<f32>,
    pub(crate) symmetry_threshold_kts: Option<f32>,
    pub(crate) data_points_threshold: Option<u32>,
    pub(crate) optimum_slant_range_nm: Option<f32>,
}

/// Minimal adapter accepted by the panel for a decoded Product 48 profile.
///
/// Product 48 files can carry several historical profiles.  The caller picks
/// the timeline entry to display and converts only that entry to this type.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Product48DisplayProfile {
    pub(crate) site_id: String,
    pub(crate) valid_time: DateTime<Utc>,
    pub(crate) radar_elevation_m: Option<f32>,
    pub(crate) source_label: String,
    pub(crate) metadata: Product48DisplayMetadata,
    pub(crate) levels: Vec<Product48DisplayLevel>,
}

#[derive(Clone, Debug)]
enum PanelProfile {
    Computed {
        profile: VwpProfile,
        dealias_label: String,
    },
    Product48(Product48DisplayProfile),
}

#[derive(Clone, Debug)]
enum PanelStatus {
    Empty,
    Computing(String),
    Ready,
    Error(String),
}

/// Persistent UI state for the docked or floating VWP viewer.
#[derive(Clone, Debug)]
pub(crate) struct VwpPanelState {
    status: PanelStatus,
    profile: Option<PanelProfile>,
}

impl Default for VwpPanelState {
    fn default() -> Self {
        Self {
            status: PanelStatus::Empty,
            profile: None,
        }
    }
}

impl VwpPanelState {
    /// Clear an old result and show a non-blocking progress state.
    pub(crate) fn begin_compute(&mut self, detail: impl Into<String>) {
        self.profile = None;
        self.status = PanelStatus::Computing(detail.into());
    }

    pub(crate) fn set_computed(&mut self, profile: VwpProfile, dealias_label: impl Into<String>) {
        self.profile = Some(PanelProfile::Computed {
            profile,
            dealias_label: dealias_label.into(),
        });
        self.status = PanelStatus::Ready;
    }

    pub(crate) fn set_product_48(&mut self, profile: Product48DisplayProfile) {
        self.profile = Some(PanelProfile::Product48(profile));
        self.status = PanelStatus::Ready;
    }

    pub(crate) fn set_error(&mut self, message: impl Into<String>) {
        self.profile = None;
        self.status = PanelStatus::Error(message.into());
    }

    pub(crate) fn clear(&mut self) {
        self.profile = None;
        self.status = PanelStatus::Empty;
    }

    /// Render the pane body and return work for the owning application.
    pub(crate) fn ui(
        &self,
        ui: &mut egui::Ui,
        storm_motion: Option<VwpMotionVector>,
    ) -> VwpPanelAction {
        let mut action = VwpPanelAction::default();
        let plot = self.profile.as_ref().map(PlotProfile::from_panel);

        ui.horizontal_wrapped(|ui| {
            if let Some(plot) = &plot {
                ui.label(egui::RichText::new(&plot.site_id).strong());
                ui.label(
                    egui::RichText::new(plot.valid_time.to_rfc3339_opts(SecondsFormat::Secs, true))
                        .monospace()
                        .color(ui.visuals().weak_text_color()),
                );
                ui.separator();
            }

            let busy = matches!(&self.status, PanelStatus::Computing(_));
            if ui
                .add_enabled(!busy, egui::Button::new("Recompute"))
                .on_hover_text("Recompute from the currently selected radar volume")
                .clicked()
            {
                action.recompute_requested = true;
            }
            if ui
                .add_enabled(
                    native_file_dialogs_available() && !busy,
                    egui::Button::new("Open Product 48..."),
                )
                .on_hover_text(if native_file_dialogs_available() {
                    "Open a NEXRAD Level III Product 48 VWP file"
                } else {
                    "Product 48 file dialogs are unavailable on this platform"
                })
                .clicked()
            {
                action.open_product48_requested = true;
            }
            if ui
                .add_enabled(
                    native_file_dialogs_available() && plot.is_some(),
                    egui::Button::new("Save CSV"),
                )
                .on_hover_text(if native_file_dialogs_available() {
                    "Save the displayed profile and its QC diagnostics"
                } else {
                    "VWP CSV file dialogs are unavailable on this platform"
                })
                .clicked()
            {
                action.save_csv_requested = true;
            }
        });

        ui.add_space(6.0);
        experimental_banner(ui);
        ui.add_space(6.0);

        match (&self.status, plot) {
            (PanelStatus::Computing(detail), _) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(detail);
                });
            }
            (PanelStatus::Error(message), _) => {
                ui.colored_label(ui.visuals().error_fg_color, message);
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "VWP needs a polar radar volume containing radial velocity. A 2-D CMAX product does not contain velocity.",
                    )
                    .color(ui.visuals().weak_text_color()),
                );
            }
            (PanelStatus::Empty, _) | (_, None) => {
                ui.label("Load a radar volume with radial velocity, then compute a VWP.");
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "The retrieval uses the selected volume's dealiased velocity; 2-D CMAX layers cannot supply a wind profile.",
                    )
                    .color(ui.visuals().weak_text_color()),
                );
            }
            (PanelStatus::Ready, Some(plot)) => profile_body(ui, &plot, storm_motion),
        }

        action
    }

    /// Serialize exactly what the pane is displaying, including rejected
    /// levels and fit diagnostics.  The caller chooses the destination.
    pub(crate) fn export_csv(&self) -> Option<String> {
        self.profile
            .as_ref()
            .map(PlotProfile::from_panel)
            .map(|profile| profile.csv())
    }
}

fn experimental_banner(ui: &mut egui::Ui) {
    let warning = ui.visuals().warn_fg_color;
    egui::Frame::new()
        .fill(warning.gamma_multiply(0.08))
        .stroke(egui::Stroke::new(1.0_f32, warning.gamma_multiply(0.75)))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("EXPERIMENTAL").strong().color(warning));
                ui.label(
                    "Wind is retrieved from radial velocity with a VAD fit. Inspect QC and do not use this product as the sole basis for decisions.",
                );
            });
        });
}

fn profile_body(ui: &mut egui::Ui, profile: &PlotProfile, storm_motion: Option<VwpMotionVector>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(&profile.source_label);
        if let Some(detail) = &profile.detail_label {
            ui.separator();
            ui.label(egui::RichText::new(detail).color(ui.visuals().weak_text_color()));
        }
        if let Some(elevation) = profile.radar_elevation_m {
            ui.separator();
            ui.label(
                egui::RichText::new(format!("radar elevation {elevation:.0} m MSL"))
                    .color(ui.visuals().weak_text_color()),
            );
        }
    });
    if let Some(metadata) = &profile.product48_metadata {
        let mut values = Vec::new();
        if let Some(threshold) = metadata.rms_threshold_kts {
            values.push(format!("RMS threshold {threshold:.1} kt"));
        }
        if let Some(threshold) = metadata.symmetry_threshold_kts {
            values.push(format!("symmetry threshold {threshold:.1} kt"));
        }
        if let Some(points) = metadata.data_points_threshold {
            values.push(format!("data-points threshold {points}"));
        }
        if let Some(range) = metadata.optimum_slant_range_nm {
            values.push(format!("optimum range {range:.1} nmi"));
        }
        if !values.is_empty() {
            ui.label(
                egui::RichText::new(values.join(" - "))
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        }
    }
    ui.add_space(5.0);
    quality_summary(ui, profile);
    wind_parameter_summary(ui, profile, storm_motion);
    ui.add_space(6.0);

    let wide = ui.available_width() >= WIDE_LAYOUT_MIN_WIDTH;
    let available_height = ui.available_height();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if wide {
                let chart_height = available_height.clamp(MIN_CHART_HEIGHT, MAX_CHART_HEIGHT);
                ui.columns(2, |columns| {
                    profile_chart(&mut columns[0], profile, chart_height);
                    hodograph_chart(&mut columns[1], profile, storm_motion, chart_height);
                });
            } else {
                profile_chart(ui, profile, NARROW_CHART_HEIGHT);
                ui.add_space(8.0);
                hodograph_chart(ui, profile, storm_motion, NARROW_CHART_HEIGHT);
            }
        });
}

fn quality_summary(ui: &mut egui::Ui, profile: &PlotProfile) {
    let (good, marginal, rejected) = profile.levels.iter().fold(
        (0_usize, 0_usize, 0_usize),
        |(good, marginal, rejected), level| match &level.outcome {
            PlotOutcome::Retrieved(wind) => match wind.quality {
                PlotQuality::Good => (good + 1, marginal, rejected),
                PlotQuality::Marginal => (good, marginal + 1, rejected),
            },
            PlotOutcome::Rejected { .. } => (good, marginal, rejected + 1),
        },
    );

    ui.horizontal_wrapped(|ui| {
        ui.colored_label(GOOD_COLOR, format!("Good {good}"));
        ui.colored_label(ui.visuals().warn_fg_color, format!("Marginal {marginal}"));
        ui.colored_label(ui.visuals().error_fg_color, format!("Rejected {rejected}"));
        ui.separator();
        ui.label(
            egui::RichText::new(format!("{} requested levels", profile.levels.len()))
                .color(ui.visuals().weak_text_color()),
        );
    });
}

fn wind_parameter_summary(
    ui: &mut egui::Ui,
    profile: &PlotProfile,
    storm_motion: Option<VwpMotionVector>,
) {
    let Some(lowest) = lowest_retrieved_vector(profile) else {
        return;
    };
    let mean_0_6 = mean_retrieved_vector(profile, 6_000.0);

    ui.horizontal_wrapped(|ui| {
        if let Some((direction, speed_kt)) = mean_0_6.map(vector_direction_speed_kt) {
            ui.label(format!("0-6 km level-mean {direction:03.0}/{speed_kt:.0} kt"))
                .on_hover_text(
                    "Component mean of the retrieved VWP winds through 6 km AGL; this is not a pressure- or density-weighted model mean wind",
                );
        }
        for (height_m, label) in [
            (1_000.0, "0-1 km shear"),
            (3_000.0, "0-3 km shear"),
            (6_000.0, "0-6 km shear"),
        ] {
            if let Some(upper) = retrieved_vector_at_height(profile, height_m) {
                let speed_kt = (upper - lowest).length() * MPS_TO_KT;
                ui.separator();
                ui.label(format!("{label} {speed_kt:.0} kt")).on_hover_text(
                    "Bulk vector difference from the lowest accepted VWP level to the named height; the VWP normally begins near 250 m AGL",
                );
            }
        }
        if let Some(motion) = storm_motion.filter(valid_motion_vector) {
            ui.separator();
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "SRV motion {:03.0}/{:.0} kt",
                    motion.direction_deg.rem_euclid(360.0),
                    motion.speed_kt
                ),
            )
            .on_hover_text(
                "The operator-selected storm-motion vector used by storm-relative velocity; plotted as a diamond on the hodograph",
            );
        }
    });
}

fn profile_chart(ui: &mut egui::Ui, profile: &PlotProfile, height: f32) {
    chart_frame(ui, "PROFILE - km AGL", height, |ui, rect, response| {
        draw_profile_chart(ui, rect, response, profile);
    });
}

fn hodograph_chart(
    ui: &mut egui::Ui,
    profile: &PlotProfile,
    storm_motion: Option<VwpMotionVector>,
    height: f32,
) {
    chart_frame(ui, "HODOGRAPH - knots", height, |ui, rect, _response| {
        draw_hodograph(ui, rect, profile, storm_motion);
    });
}

fn chart_frame(
    ui: &mut egui::Ui,
    title: &str,
    height: f32,
    draw: impl FnOnce(&mut egui::Ui, egui::Rect, egui::Response),
) {
    let visuals = ui.visuals().clone();
    egui::Frame::new()
        .fill(visuals.extreme_bg_color)
        .stroke(visuals.widgets.noninteractive.bg_stroke)
        .corner_radius(4)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(title)
                    .small()
                    .strong()
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(3.0);
            let size = egui::vec2(ui.available_width().max(160.0), height.max(220.0));
            let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
            draw(ui, rect, response);
        });
}

fn draw_profile_chart(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    response: egui::Response,
    profile: &PlotProfile,
) {
    let visuals = ui.visuals().clone();
    let painter = ui.painter_at(rect);
    let plot_rect = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(42.0, 10.0),
        rect.right_bottom() - egui::vec2(10.0, 20.0),
    );
    if plot_rect.width() <= 20.0 || plot_rect.height() <= 20.0 {
        return;
    }

    let max_height_m = profile
        .levels
        .iter()
        .map(|level| level.target_height_m_agl)
        .filter(|height| height.is_finite())
        .fold(0.0_f32, f32::max)
        .max(3_000.0);
    let ceiling_m = (max_height_m / 2_000.0).ceil() * 2_000.0;
    let grid_step_m = if ceiling_m <= 6_000.0 {
        1_000.0
    } else if ceiling_m <= 14_000.0 {
        2_000.0
    } else {
        3_000.0
    };
    let height_y = |height_m: f32| {
        plot_rect.bottom() - (height_m / ceiling_m).clamp(0.0, 1.0) * plot_rect.height()
    };

    let grid_color = visuals
        .widgets
        .noninteractive
        .bg_stroke
        .color
        .gamma_multiply(0.7);
    let weak = visuals.weak_text_color();
    let mut grid_height = 0.0;
    while grid_height <= ceiling_m + 0.5 {
        let y = height_y(grid_height);
        painter.line_segment(
            [
                egui::pos2(plot_rect.left(), y),
                egui::pos2(plot_rect.right(), y),
            ],
            egui::Stroke::new(1.0_f32, grid_color),
        );
        painter.text(
            egui::pos2(plot_rect.left() - 7.0, y),
            egui::Align2::RIGHT_CENTER,
            format!("{:.0}", grid_height / 1_000.0),
            egui::FontId::monospace(10.0),
            weak,
        );
        grid_height += grid_step_m;
    }

    let staff_origin_x = plot_rect.center().x;
    painter.line_segment(
        [
            egui::pos2(staff_origin_x, plot_rect.top()),
            egui::pos2(staff_origin_x, plot_rect.bottom()),
        ],
        egui::Stroke::new(1.0_f32, grid_color.gamma_multiply(0.7)),
    );

    for level in &profile.levels {
        let y = height_y(level.target_height_m_agl);
        let origin = egui::pos2(staff_origin_x, y);
        match &level.outcome {
            PlotOutcome::Retrieved(wind) => {
                let color = match wind.quality {
                    PlotQuality::Good => visuals.text_color(),
                    PlotQuality::Marginal => visuals.warn_fg_color,
                };
                draw_wind_barb(&painter, origin, wind.direction_deg, wind.speed_mps, color);
                painter.circle_filled(
                    origin,
                    2.0,
                    match wind.quality {
                        PlotQuality::Good => GOOD_COLOR,
                        PlotQuality::Marginal => visuals.warn_fg_color,
                    },
                );
            }
            PlotOutcome::Rejected { .. } => {
                let color = visuals.error_fg_color.gamma_multiply(0.75);
                painter.line_segment(
                    [origin - egui::vec2(3.0, 3.0), origin + egui::vec2(3.0, 3.0)],
                    egui::Stroke::new(1.2_f32, color),
                );
                painter.line_segment(
                    [
                        origin + egui::vec2(-3.0, 3.0),
                        origin + egui::vec2(3.0, -3.0),
                    ],
                    egui::Stroke::new(1.2_f32, color),
                );
            }
        }
    }

    let hovered_level = ui
        .ctx()
        .pointer_hover_pos()
        .filter(|position| response.hovered() && plot_rect.contains(*position))
        .and_then(|position| {
            profile.levels.iter().min_by(|left, right| {
                let left_distance = (height_y(left.target_height_m_agl) - position.y).abs();
                let right_distance = (height_y(right.target_height_m_agl) - position.y).abs();
                left_distance.total_cmp(&right_distance)
            })
        });
    if let Some(level) = hovered_level {
        response.on_hover_ui(|ui| level_hover_ui(ui, level));
    }
}

fn draw_wind_barb(
    painter: &egui::Painter,
    origin: egui::Pos2,
    direction_deg: f32,
    speed_mps: f32,
    color: egui::Color32,
) {
    let knots = (speed_mps.max(0.0) * MPS_TO_KT).round();
    if knots < 2.5 {
        painter.circle_stroke(origin, 3.0, egui::Stroke::new(1.2_f32, color));
        return;
    }

    let radians = direction_deg.to_radians();
    let toward_source = egui::vec2(radians.sin(), -radians.cos());
    let feather_side = egui::vec2(-toward_source.y, toward_source.x);
    let tip = origin + toward_source * 23.0;
    painter.line_segment([origin, tip], egui::Stroke::new(1.35_f32, color));

    let inward = -toward_source;
    let mut cursor = tip;
    let mut five_knot_units = ((knots + 2.5) / 5.0).floor() as i32;
    while five_knot_units >= 10 {
        let inside = cursor + inward * 6.0;
        let flag_tip = cursor + feather_side * 8.0 + inward * 3.0;
        painter.add(egui::Shape::convex_polygon(
            vec![cursor, flag_tip, inside],
            color,
            egui::Stroke::NONE,
        ));
        cursor = inside + inward * 1.5;
        five_knot_units -= 10;
    }
    while five_knot_units >= 2 {
        painter.line_segment(
            [cursor, cursor + feather_side * 8.0 + inward * 3.0],
            egui::Stroke::new(1.35_f32, color),
        );
        cursor += inward * 4.0;
        five_knot_units -= 2;
    }
    if five_knot_units == 1 {
        painter.line_segment(
            [cursor, cursor + feather_side * 4.5 + inward * 1.5],
            egui::Stroke::new(1.35_f32, color),
        );
    }
}

fn level_hover_ui(ui: &mut egui::Ui, level: &PlotLevel) {
    ui.label(
        egui::RichText::new(format!("{:.2} km AGL", level.target_height_m_agl / 1_000.0)).strong(),
    );
    match &level.outcome {
        PlotOutcome::Retrieved(wind) => {
            ui.label(format!(
                "{:03.0} deg at {:.1} kt",
                wind.direction_deg.rem_euclid(360.0),
                wind.speed_mps * MPS_TO_KT
            ));
            ui.label(format!("u {:.1}, v {:.1} m/s", wind.u_mps, wind.v_mps));
            if let Some(rms) = wind.rms_mps {
                ui.label(format!(
                    "fit RMS {:.1} m/s ({:.1} kt)",
                    rms,
                    rms * MPS_TO_KT
                ));
            }
            if let Some(divergence) = wind.product48_divergence {
                ui.label(format!("Product 48 divergence {divergence:.4}"));
            }
            if let (Some(samples), Some(sectors)) = (wind.samples_used, wind.azimuth_sectors) {
                ui.label(format!(
                    "{samples} samples across {sectors}/12 azimuth sectors"
                ));
            }
            if let Some(gap) = wind.max_azimuth_gap_deg {
                ui.label(format!("largest azimuth gap {gap:.0} deg"));
            }
            if let (Some(range), Some(elevation)) = (wind.slant_range_m, wind.elevation_deg) {
                ui.label(format!(
                    "range {:.1} km at {elevation:.1} deg",
                    range / 1_000.0
                ));
            }
        }
        PlotOutcome::Rejected {
            reason,
            diagnostics,
        } => {
            ui.colored_label(ui.visuals().error_fg_color, format!("Rejected: {reason}"));
            if let Some(diagnostics) = diagnostics {
                ui.label(format!(
                    "{} samples, {}/12 sectors, {:.0} deg largest gap",
                    diagnostics.samples_used,
                    diagnostics.azimuth_sectors,
                    diagnostics.max_azimuth_gap_deg
                ));
                if let Some(rms) = diagnostics.rms_mps {
                    ui.label(format!("candidate RMS {rms:.1} m/s"));
                }
            }
        }
    }
}

fn draw_hodograph(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    profile: &PlotProfile,
    storm_motion: Option<VwpMotionVector>,
) {
    let visuals = ui.visuals().clone();
    let painter = ui.painter_at(rect);
    let winds = profile
        .levels
        .iter()
        .filter_map(|level| match &level.outcome {
            PlotOutcome::Retrieved(wind) => Some((level.target_height_m_agl, wind)),
            PlotOutcome::Rejected { .. } => None,
        })
        .collect::<Vec<_>>();

    let side = rect.width().min(rect.height()).max(1.0);
    let radius = (side * 0.5 - 32.0).max(20.0);
    let center = rect.center();
    let maximum_wind_kt = winds
        .iter()
        .map(|(_, wind)| wind.speed_mps * MPS_TO_KT)
        .filter(|speed| speed.is_finite())
        .fold(0.0_f32, f32::max)
        .max(
            storm_motion
                .filter(valid_motion_vector)
                .map_or(0.0, |motion| motion.speed_kt),
        );
    let ring_step_kt = if maximum_wind_kt <= 40.0 {
        10.0
    } else if maximum_wind_kt <= 80.0 {
        20.0
    } else {
        25.0
    };
    let scale_kt = (maximum_wind_kt.max(20.0) / ring_step_kt).ceil() * ring_step_kt;
    let grid_color = visuals
        .widgets
        .noninteractive
        .bg_stroke
        .color
        .gamma_multiply(0.8);
    let weak = visuals.weak_text_color();

    let mut ring_kt = ring_step_kt;
    while ring_kt <= scale_kt + 0.5 {
        let ring_radius = radius * ring_kt / scale_kt;
        painter.circle_stroke(center, ring_radius, egui::Stroke::new(1.0_f32, grid_color));
        painter.text(
            center + egui::vec2(3.0, -ring_radius + 2.0),
            egui::Align2::LEFT_TOP,
            format!("{ring_kt:.0}"),
            egui::FontId::monospace(9.0),
            weak,
        );
        ring_kt += ring_step_kt;
    }
    painter.line_segment(
        [
            center - egui::vec2(radius, 0.0),
            center + egui::vec2(radius, 0.0),
        ],
        egui::Stroke::new(1.0_f32, grid_color),
    );
    painter.line_segment(
        [
            center - egui::vec2(0.0, radius),
            center + egui::vec2(0.0, radius),
        ],
        egui::Stroke::new(1.0_f32, grid_color),
    );
    painter.text(
        center - egui::vec2(0.0, radius + 8.0),
        egui::Align2::CENTER_BOTTOM,
        "N",
        egui::FontId::monospace(10.0),
        weak,
    );
    painter.text(
        center + egui::vec2(radius + 8.0, 0.0),
        egui::Align2::LEFT_CENTER,
        "E",
        egui::FontId::monospace(10.0),
        weak,
    );
    painter.text(
        center + egui::vec2(0.0, radius + 8.0),
        egui::Align2::CENTER_TOP,
        "S",
        egui::FontId::monospace(10.0),
        weak,
    );
    painter.text(
        center - egui::vec2(radius + 8.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        "W",
        egui::FontId::monospace(10.0),
        weak,
    );

    let maximum_height_m = winds
        .iter()
        .map(|(height, _)| *height)
        .fold(0.0_f32, f32::max)
        .max(1.0);
    let wind_position = |wind: &PlotWind| {
        center
            + egui::vec2(
                wind.u_mps * MPS_TO_KT / scale_kt * radius,
                -wind.v_mps * MPS_TO_KT / scale_kt * radius,
            )
    };

    for pair in winds.windows(2) {
        let (lower_height, lower_wind) = pair[0];
        let (upper_height, upper_wind) = pair[1];
        let color = height_color(((lower_height + upper_height) * 0.5) / maximum_height_m);
        painter.line_segment(
            [wind_position(lower_wind), wind_position(upper_wind)],
            egui::Stroke::new(2.4_f32, color),
        );
    }
    for (height, wind) in winds {
        let color = match wind.quality {
            PlotQuality::Good => height_color(height / maximum_height_m),
            PlotQuality::Marginal => visuals.warn_fg_color,
        };
        painter.circle_filled(wind_position(wind), 2.8, color);
    }

    if let Some(motion) = storm_motion.filter(valid_motion_vector) {
        let direction_rad = motion.direction_deg.to_radians();
        let u_kt = -motion.speed_kt * direction_rad.sin();
        let v_kt = -motion.speed_kt * direction_rad.cos();
        let position = center + egui::vec2(u_kt / scale_kt * radius, -v_kt / scale_kt * radius);
        let color = visuals.warn_fg_color;
        painter.line_segment([center, position], egui::Stroke::new(1.3_f32, color));
        let diamond = [
            position + egui::vec2(0.0, -5.0),
            position + egui::vec2(5.0, 0.0),
            position + egui::vec2(0.0, 5.0),
            position + egui::vec2(-5.0, 0.0),
        ];
        painter.add(egui::Shape::convex_polygon(
            diamond.to_vec(),
            color,
            egui::Stroke::new(1.0_f32, visuals.extreme_bg_color),
        ));
        painter.text(
            position + egui::vec2(7.0, -7.0),
            egui::Align2::LEFT_BOTTOM,
            format!(
                "SRV {:03.0}/{:.0}",
                motion.direction_deg.rem_euclid(360.0),
                motion.speed_kt
            ),
            egui::FontId::monospace(9.0),
            color,
        );
    }

    if maximum_wind_kt <= f32::EPSILON {
        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            "No retrieved winds",
            egui::FontId::proportional(12.0),
            weak,
        );
    }
}

fn valid_motion_vector(motion: &VwpMotionVector) -> bool {
    motion.direction_deg.is_finite() && motion.speed_kt.is_finite() && motion.speed_kt >= 0.0
}

fn lowest_retrieved_vector(profile: &PlotProfile) -> Option<egui::Vec2> {
    profile
        .levels
        .iter()
        .find_map(|level| match &level.outcome {
            PlotOutcome::Retrieved(wind) => Some(egui::vec2(wind.u_mps, wind.v_mps)),
            PlotOutcome::Rejected { .. } => None,
        })
}

fn retrieved_vector_at_height(profile: &PlotProfile, target_height_m: f32) -> Option<egui::Vec2> {
    let winds = profile
        .levels
        .iter()
        .filter_map(|level| match &level.outcome {
            PlotOutcome::Retrieved(wind) => Some((
                level.target_height_m_agl,
                egui::vec2(wind.u_mps, wind.v_mps),
            )),
            PlotOutcome::Rejected { .. } => None,
        })
        .collect::<Vec<_>>();
    for pair in winds.windows(2) {
        let (lower_height, lower) = pair[0];
        let (upper_height, upper) = pair[1];
        if target_height_m < lower_height || target_height_m > upper_height {
            continue;
        }
        // Do not silently interpolate through a deep rejected layer.
        if upper_height - lower_height > 1_000.0 {
            return None;
        }
        let fraction = if upper_height > lower_height {
            (target_height_m - lower_height) / (upper_height - lower_height)
        } else {
            0.0
        };
        return Some(lower + (upper - lower) * fraction);
    }
    winds
        .into_iter()
        .find(|(height, _)| (*height - target_height_m).abs() <= 1.0)
        .map(|(_, wind)| wind)
}

fn mean_retrieved_vector(profile: &PlotProfile, maximum_height_m: f32) -> Option<egui::Vec2> {
    let (sum, count) = profile
        .levels
        .iter()
        .filter(|level| level.target_height_m_agl <= maximum_height_m)
        .filter_map(|level| match &level.outcome {
            PlotOutcome::Retrieved(wind) => Some(egui::vec2(wind.u_mps, wind.v_mps)),
            PlotOutcome::Rejected { .. } => None,
        })
        .fold((egui::Vec2::ZERO, 0_u32), |(sum, count), wind| {
            (sum + wind, count + 1)
        });
    (count >= 2).then(|| sum / count as f32)
}

fn vector_direction_speed_kt(vector: egui::Vec2) -> (f32, f32) {
    let speed_mps = vector.length();
    let direction_deg = (-vector.x).atan2(-vector.y).to_degrees().rem_euclid(360.0);
    (direction_deg, speed_mps * MPS_TO_KT)
}

fn height_color(fraction: f32) -> egui::Color32 {
    let fraction = fraction.clamp(0.0, 1.0);
    let mix =
        |low: u8, high: u8| (f32::from(low) + (f32::from(high) - f32::from(low)) * fraction) as u8;
    egui::Color32::from_rgb(
        mix(HODO_LOW_COLOR.r(), HODO_HIGH_COLOR.r()),
        mix(HODO_LOW_COLOR.g(), HODO_HIGH_COLOR.g()),
        mix(HODO_LOW_COLOR.b(), HODO_HIGH_COLOR.b()),
    )
}

#[derive(Clone, Debug)]
struct PlotProfile {
    site_id: String,
    valid_time: DateTime<Utc>,
    radar_elevation_m: Option<f32>,
    source_label: String,
    detail_label: Option<String>,
    product48_metadata: Option<Product48DisplayMetadata>,
    levels: Vec<PlotLevel>,
}

impl PlotProfile {
    fn from_panel(panel: &PanelProfile) -> Self {
        match panel {
            PanelProfile::Computed {
                profile,
                dealias_label,
            } => Self {
                site_id: profile.site_id.clone(),
                valid_time: profile.valid_time,
                radar_elevation_m: profile.radar_elevation_m,
                source_label: "VAD fit from dealiased radial velocity".to_owned(),
                detail_label: Some(format!(
                    "{dealias_label} - {} velocity cuts",
                    profile.velocity_cut_count
                )),
                product48_metadata: None,
                levels: profile
                    .levels
                    .iter()
                    .map(|level| PlotLevel {
                        target_height_m_agl: level.target_height_m_agl,
                        outcome: match &level.outcome {
                            VwpLevelOutcome::Retrieved(wind) => PlotOutcome::Retrieved(PlotWind {
                                height_m_msl: wind.height_m_msl,
                                u_mps: wind.u_mps,
                                v_mps: wind.v_mps,
                                direction_deg: wind.direction_deg,
                                speed_mps: wind.speed_mps,
                                rms_mps: wind.diagnostics.rms_mps,
                                product48_divergence: None,
                                quality: match wind.quality {
                                    VwpQuality::Good => PlotQuality::Good,
                                    VwpQuality::Marginal => PlotQuality::Marginal,
                                },
                                samples_used: Some(wind.diagnostics.samples_used),
                                azimuth_sectors: Some(wind.diagnostics.azimuth_sectors),
                                max_azimuth_gap_deg: Some(wind.diagnostics.max_azimuth_gap_deg),
                                slant_range_m: Some(wind.diagnostics.slant_range_m),
                                elevation_deg: Some(wind.diagnostics.elevation_deg),
                            }),
                            VwpLevelOutcome::Rejected(rejected) => PlotOutcome::Rejected {
                                reason: rejection_label(rejected.reason).to_owned(),
                                diagnostics: rejected.best_candidate.as_ref().map(|candidate| {
                                    RejectedDiagnostics {
                                        samples_used: candidate.samples_used,
                                        azimuth_sectors: candidate.azimuth_sectors,
                                        max_azimuth_gap_deg: candidate.max_azimuth_gap_deg,
                                        rms_mps: candidate.rms_mps,
                                    }
                                }),
                            },
                        },
                    })
                    .collect(),
            },
            PanelProfile::Product48(profile) => Self {
                site_id: profile.site_id.clone(),
                valid_time: profile.valid_time,
                radar_elevation_m: profile.radar_elevation_m,
                source_label: profile.source_label.clone(),
                detail_label: Some("NEXRAD Level III Product 48".to_owned()),
                product48_metadata: Some(profile.metadata.clone()),
                levels: profile
                    .levels
                    .iter()
                    .map(|level| PlotLevel {
                        target_height_m_agl: level.height_m_agl,
                        outcome: match &level.outcome {
                            Product48DisplayOutcome::Retrieved {
                                direction_deg,
                                speed_mps,
                                rms_mps,
                                divergence,
                                slant_range_nm,
                                elevation_deg,
                                quality,
                            } => {
                                let direction_rad = direction_deg.to_radians();
                                PlotOutcome::Retrieved(PlotWind {
                                    height_m_msl: level.height_m_msl,
                                    u_mps: -speed_mps * direction_rad.sin(),
                                    v_mps: -speed_mps * direction_rad.cos(),
                                    direction_deg: *direction_deg,
                                    speed_mps: *speed_mps,
                                    rms_mps: *rms_mps,
                                    product48_divergence: *divergence,
                                    quality: match quality {
                                        Product48DisplayQuality::Good => PlotQuality::Good,
                                        Product48DisplayQuality::Marginal => PlotQuality::Marginal,
                                    },
                                    samples_used: None,
                                    azimuth_sectors: None,
                                    max_azimuth_gap_deg: None,
                                    slant_range_m: slant_range_nm.map(|range| range * NM_TO_M),
                                    elevation_deg: *elevation_deg,
                                })
                            }
                            Product48DisplayOutcome::Rejected { reason } => PlotOutcome::Rejected {
                                reason: reason.clone(),
                                diagnostics: None,
                            },
                        },
                    })
                    .collect(),
            },
        }
    }

    fn csv(&self) -> String {
        let mut output = String::from(
            "site_id,valid_time,source,target_height_m_agl,height_m_msl,direction_deg,speed_mps,speed_kt,u_mps,v_mps,rms_mps,product48_divergence,quality,rejection_reason,samples_used,azimuth_sectors,max_azimuth_gap_deg,slant_range_m,elevation_deg,product48_rms_threshold_kt,product48_symmetry_threshold_kt,product48_data_points_threshold,product48_optimum_slant_range_nm\n",
        );
        let site_id = csv_escape(&self.site_id);
        let valid_time = self.valid_time.to_rfc3339_opts(SecondsFormat::Secs, true);
        let source = csv_escape(&self.source_label);
        let metadata = self.product48_metadata.as_ref();
        let rms_threshold = optional_f32(metadata.and_then(|value| value.rms_threshold_kts), 2);
        let symmetry_threshold =
            optional_f32(metadata.and_then(|value| value.symmetry_threshold_kts), 2);
        let data_points_threshold = metadata
            .and_then(|value| value.data_points_threshold)
            .map_or_else(String::new, |value| value.to_string());
        let optimum_range =
            optional_f32(metadata.and_then(|value| value.optimum_slant_range_nm), 2);

        for level in &self.levels {
            let fields = match &level.outcome {
                PlotOutcome::Retrieved(wind) => {
                    let quality = match wind.quality {
                        PlotQuality::Good => "good",
                        PlotQuality::Marginal => "marginal",
                    };
                    [
                        site_id.clone(),
                        valid_time.clone(),
                        source.clone(),
                        format!("{:.1}", level.target_height_m_agl),
                        optional_f32(wind.height_m_msl, 1),
                        format!("{:.1}", wind.direction_deg),
                        format!("{:.3}", wind.speed_mps),
                        format!("{:.3}", wind.speed_mps * MPS_TO_KT),
                        format!("{:.3}", wind.u_mps),
                        format!("{:.3}", wind.v_mps),
                        optional_f32(wind.rms_mps, 3),
                        optional_f32(wind.product48_divergence, 4),
                        quality.to_owned(),
                        String::new(),
                        optional_usize(wind.samples_used),
                        optional_usize(wind.azimuth_sectors),
                        optional_f32(wind.max_azimuth_gap_deg, 1),
                        optional_f32(wind.slant_range_m, 1),
                        optional_f32(wind.elevation_deg, 2),
                        rms_threshold.clone(),
                        symmetry_threshold.clone(),
                        data_points_threshold.clone(),
                        optimum_range.clone(),
                    ]
                }
                PlotOutcome::Rejected {
                    reason,
                    diagnostics,
                } => {
                    let (samples, sectors, gap, rms) = diagnostics.as_ref().map_or_else(
                        || (String::new(), String::new(), String::new(), String::new()),
                        |diagnostics| {
                            (
                                diagnostics.samples_used.to_string(),
                                diagnostics.azimuth_sectors.to_string(),
                                format!("{:.1}", diagnostics.max_azimuth_gap_deg),
                                optional_f32(diagnostics.rms_mps, 3),
                            )
                        },
                    );
                    [
                        site_id.clone(),
                        valid_time.clone(),
                        source.clone(),
                        format!("{:.1}", level.target_height_m_agl),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        rms,
                        String::new(),
                        "rejected".to_owned(),
                        csv_escape(reason),
                        samples,
                        sectors,
                        gap,
                        String::new(),
                        String::new(),
                        rms_threshold.clone(),
                        symmetry_threshold.clone(),
                        data_points_threshold.clone(),
                        optimum_range.clone(),
                    ]
                }
            };
            let _ = writeln!(output, "{}", fields.join(","));
        }
        output
    }
}

#[derive(Clone, Debug)]
struct PlotLevel {
    target_height_m_agl: f32,
    outcome: PlotOutcome,
}

#[derive(Clone, Debug)]
enum PlotOutcome {
    Retrieved(PlotWind),
    Rejected {
        reason: String,
        diagnostics: Option<RejectedDiagnostics>,
    },
}

#[derive(Clone, Debug)]
struct PlotWind {
    height_m_msl: Option<f32>,
    u_mps: f32,
    v_mps: f32,
    direction_deg: f32,
    speed_mps: f32,
    rms_mps: Option<f32>,
    product48_divergence: Option<f32>,
    quality: PlotQuality,
    samples_used: Option<usize>,
    azimuth_sectors: Option<usize>,
    max_azimuth_gap_deg: Option<f32>,
    slant_range_m: Option<f32>,
    elevation_deg: Option<f32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlotQuality {
    Good,
    Marginal,
}

#[derive(Clone, Debug)]
struct RejectedDiagnostics {
    samples_used: usize,
    azimuth_sectors: usize,
    max_azimuth_gap_deg: f32,
    rms_mps: Option<f32>,
}

fn rejection_label(reason: VwpRejectionReason) -> &'static str {
    match reason {
        VwpRejectionReason::NoBeamCoverage => "no beam coverage",
        VwpRejectionReason::InsufficientSamples => "insufficient samples",
        VwpRejectionReason::InsufficientAzimuthCoverage => "insufficient azimuth coverage",
        VwpRejectionReason::IllConditionedFit => "ill-conditioned fit",
        VwpRejectionReason::ExcessiveOutliers => "excessive outliers",
        VwpRejectionReason::ResidualTooLarge => "fit residual too large",
    }
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn optional_f32(value: Option<f32>, precision: usize) -> String {
    value.map_or_else(String::new, |value| format!("{value:.precision$}"))
}

fn optional_usize(value: Option<usize>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    #[test]
    fn product_48_adapter_obeys_meteorological_direction_convention() {
        let profile = Product48DisplayProfile {
            site_id: "KBMX".to_owned(),
            valid_time: Utc
                .with_ymd_and_hms(1998, 4, 16, 0, 6, 45)
                .single()
                .unwrap(),
            radar_elevation_m: Some(231.3),
            source_label: "Product 48 symbology".to_owned(),
            metadata: Product48DisplayMetadata::default(),
            levels: vec![Product48DisplayLevel {
                height_m_agl: 73.0,
                height_m_msl: Some(304.8),
                outcome: Product48DisplayOutcome::Retrieved {
                    direction_deg: 180.0,
                    speed_mps: 10.0,
                    rms_mps: Some(3.0),
                    divergence: None,
                    slant_range_nm: None,
                    elevation_deg: None,
                    quality: Product48DisplayQuality::Good,
                },
            }],
        };

        let plot = PlotProfile::from_panel(&PanelProfile::Product48(profile));
        let PlotOutcome::Retrieved(wind) = &plot.levels[0].outcome else {
            panic!("expected a retrieved level");
        };
        assert!(wind.u_mps.abs() < 0.001);
        assert!((wind.v_mps - 10.0).abs() < 0.001);
    }

    #[test]
    fn csv_preserves_rejected_levels_and_quotes_source() {
        let profile = Product48DisplayProfile {
            site_id: "KBMX".to_owned(),
            valid_time: Utc
                .with_ymd_and_hms(1998, 4, 16, 0, 6, 45)
                .single()
                .unwrap(),
            radar_elevation_m: Some(231.3),
            source_label: "Product 48, symbology".to_owned(),
            metadata: Product48DisplayMetadata::default(),
            levels: vec![Product48DisplayLevel {
                height_m_agl: 500.0,
                height_m_msl: None,
                outcome: Product48DisplayOutcome::Rejected {
                    reason: "RMS, too large".to_owned(),
                },
            }],
        };
        let mut state = VwpPanelState::default();
        state.set_product_48(profile);

        let csv = state.export_csv().unwrap();
        assert!(csv.contains("\"Product 48, symbology\""));
        assert!(csv.contains("rejected,\"RMS, too large\""));
    }

    #[test]
    fn product_48_csv_preserves_tabular_diagnostics_and_thresholds() {
        let profile = Product48DisplayProfile {
            site_id: "KTLX".to_owned(),
            valid_time: Utc
                .with_ymd_and_hms(2026, 7, 10, 20, 0, 0)
                .single()
                .unwrap(),
            radar_elevation_m: Some(370.0),
            source_label: "Product 48 tabular VAD output".to_owned(),
            metadata: Product48DisplayMetadata {
                rms_threshold_kts: Some(9.7),
                symmetry_threshold_kts: Some(13.6),
                data_points_threshold: Some(25),
                optimum_slant_range_nm: Some(16.2),
            },
            levels: vec![Product48DisplayLevel {
                height_m_agl: 1_000.0,
                height_m_msl: Some(1_370.0),
                outcome: Product48DisplayOutcome::Retrieved {
                    direction_deg: 225.0,
                    speed_mps: 15.0,
                    rms_mps: Some(2.0),
                    divergence: Some(0.0012),
                    slant_range_nm: Some(16.2),
                    elevation_deg: Some(2.5),
                    quality: Product48DisplayQuality::Good,
                },
            }],
        };
        let mut state = VwpPanelState::default();
        state.set_product_48(profile);

        let csv = state.export_csv().unwrap();
        assert!(csv.contains("product48_divergence"));
        assert!(csv.contains("product48_rms_threshold_kt"));
        assert!(csv.contains(",0.0012,good,"));
        assert!(csv.contains(",30002.4,2.50,9.70,13.60,25,16.20"));
    }

    #[test]
    fn native_dialog_actions_follow_the_desktop_target_gate() {
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        assert!(native_file_dialogs_available());
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        assert!(!native_file_dialogs_available());
    }

    #[test]
    fn clear_drops_csv_export() {
        let mut state = VwpPanelState::default();
        state.begin_compute("computing");
        state.clear();
        assert!(state.export_csv().is_none());
    }

    fn parameter_test_profile(levels: &[(f32, f32, f32)]) -> PlotProfile {
        PlotProfile {
            site_id: "KTLX".to_owned(),
            valid_time: Utc.timestamp_millis_opt(0).single().unwrap(),
            radar_elevation_m: None,
            source_label: "test".to_owned(),
            detail_label: None,
            product48_metadata: None,
            levels: levels
                .iter()
                .map(|(height, u_mps, v_mps)| PlotLevel {
                    target_height_m_agl: *height,
                    outcome: PlotOutcome::Retrieved(PlotWind {
                        height_m_msl: None,
                        u_mps: *u_mps,
                        v_mps: *v_mps,
                        direction_deg: 0.0,
                        speed_mps: u_mps.hypot(*v_mps),
                        rms_mps: None,
                        product48_divergence: None,
                        quality: PlotQuality::Good,
                        samples_used: None,
                        azimuth_sectors: None,
                        max_azimuth_gap_deg: None,
                        slant_range_m: None,
                        elevation_deg: None,
                    }),
                })
                .collect(),
        }
    }

    #[test]
    fn vwp_parameter_vectors_interpolate_components_and_report_direction() {
        let profile =
            parameter_test_profile(&[(250.0, 0.0, 5.0), (750.0, 5.0, 5.0), (1_250.0, 10.0, 5.0)]);

        let at_one_km = retrieved_vector_at_height(&profile, 1_000.0).unwrap();
        assert!((at_one_km.x - 7.5).abs() < 0.001);
        assert!((at_one_km.y - 5.0).abs() < 0.001);
        let (direction, speed_kt) = vector_direction_speed_kt(egui::vec2(0.0, 10.0));
        assert!((direction - 180.0).abs() < 0.001);
        assert!((speed_kt - 19.438_444).abs() < 0.001);
    }

    #[test]
    fn vwp_parameter_vectors_do_not_bridge_deep_qc_gaps() {
        let profile = parameter_test_profile(&[(250.0, 0.0, 5.0), (2_000.0, 10.0, 5.0)]);

        assert!(retrieved_vector_at_height(&profile, 1_000.0).is_none());
    }
}
