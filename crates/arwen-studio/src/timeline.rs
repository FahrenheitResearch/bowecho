// SPDX-License-Identifier: Apache-2.0

//! The bottom of the workspace: TWO tracks, never merged (binding
//! design rule) — the pipeline stage strip (what the run is doing) and
//! the forecast valid-time timeline (what the forecast covers).
//! Timeline widgets return actions; they never load data (BowEcho
//! live_archive_bar convention).

use eframe::egui;

use crate::kit;
use crate::run_session::{RunSession, StageStatus};
use crate::theme::theme;

#[derive(Debug, Default)]
pub struct TimelineActions {
    /// `Some(Some(i))` = select frame i; `Some(None)` = follow latest.
    pub select_frame: Option<Option<usize>>,
}

/// The five-stage pipeline strip.
pub fn stages_ui(ui: &mut egui::Ui, session: Option<&RunSession>) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("PIPELINE")
                .size(10.0)
                .color(theme().subhead),
        );
        let Some(session) = session else {
            ui.label(egui::RichText::new("no run").weak().size(11.0));
            return;
        };
        for stage in arwen_plan::STAGES {
            let state = session
                .stages
                .iter()
                .rev()
                .find(|candidate| candidate.id == stage);
            let (color, strong) = match state.map(|state| state.status) {
                None => (theme().text_weak, false),
                Some(StageStatus::Running) => (theme().accent, true),
                Some(StageStatus::Ok) => (theme().live, false),
                Some(StageStatus::Failed) => (theme().alert, true),
            };
            let mut text = egui::RichText::new(stage).size(11.0).color(color);
            if strong {
                text = text.strong();
            }
            let mut response = ui.label(text);
            if let Some(state) = state {
                let mut hover = String::new();
                if let Some(seconds) = state.wall_seconds {
                    hover.push_str(&format!("{}\n", kit::format_duration_s(seconds)));
                }
                if !state.phases.is_empty() {
                    hover.push_str(&state.phases.join(" → "));
                }
                if !hover.is_empty() {
                    response = response.on_hover_text(hover);
                }
            }
            // Separator dot between stages.
            if stage != "finalize" {
                ui.label(egui::RichText::new("·").weak());
            }
            let _ = response;
        }
        // Forecast fraction + speed on the same strip, right-aligned.
        if session.terminal.is_none() && session.progress.model_seconds > 0.0 {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(speed) = session.progress.speed_x {
                    ui.label(kit::value_text(&format!("{speed:.1}×")));
                }
                if let Some(fraction) = session.progress.fraction() {
                    ui.add(
                        egui::ProgressBar::new(fraction as f32)
                            .desired_width(140.0)
                            .desired_height(10.0),
                    );
                }
                ui.label(kit::value_text(&format!(
                    "t+{}",
                    kit::format_duration_s(session.progress.model_seconds)
                )));
            });
        }
    });
}

/// The forecast valid-time track: one tick per committed frame, click or
/// drag to scrub, follow-latest chip at the right.
pub fn valid_time_ui(
    ui: &mut egui::Ui,
    session: Option<&RunSession>,
    actions: &mut TimelineActions,
) {
    let Some(session) = session else {
        ui.label(
            egui::RichText::new("VALID TIME — forecast frames appear here as they commit")
                .size(10.0)
                .color(theme().text_weak),
        );
        return;
    };
    if session.outputs.is_empty() {
        ui.label(
            egui::RichText::new("VALID TIME — no frames committed yet")
                .size(10.0)
                .color(theme().text_weak),
        );
        return;
    }

    ui.horizontal(|ui| {
        let first = session.outputs.first().expect("nonempty");
        let last = session.outputs.last().expect("nonempty");
        ui.label(kit::value_text(
            &first.valid_time_utc.format("%H:%MZ").to_string(),
        ));

        // The scrub lane.
        let lane_width = (ui.available_width() - 120.0).max(60.0);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(lane_width, 18.0), egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.line_segment(
            [
                egui::pos2(rect.left() + 2.0, rect.center().y),
                egui::pos2(rect.right() - 2.0, rect.center().y),
            ],
            egui::Stroke::new(1.0, theme().hairline),
        );
        let count = session.outputs.len();
        let x_for = |index: usize| {
            if count == 1 {
                rect.center().x
            } else {
                rect.left() + 2.0 + (rect.width() - 4.0) * index as f32 / (count - 1) as f32
            }
        };
        let selected_index = session.display_frame().map(|(index, _)| index);
        for index in 0..count {
            let x = x_for(index);
            let selected = Some(index) == selected_index;
            let (radius, color) = if selected {
                (4.5, theme().accent)
            } else {
                (2.5, theme().text_weak)
            };
            painter.circle_filled(egui::pos2(x, rect.center().y), radius, color);
        }
        // Click/drag scrubbing: nearest tick to the pointer.
        if response.clicked() || response.dragged() {
            if let Some(pointer) = response.interact_pointer_pos() {
                let mut best = 0usize;
                let mut best_distance = f32::INFINITY;
                for index in 0..count {
                    let distance = (x_for(index) - pointer.x).abs();
                    if distance < best_distance {
                        best_distance = distance;
                        best = index;
                    }
                }
                actions.select_frame = Some(Some(best));
            }
        }
        if let Some(pointer) = response.hover_pos() {
            let mut best = 0usize;
            let mut best_distance = f32::INFINITY;
            for index in 0..count {
                let distance = (x_for(index) - pointer.x).abs();
                if distance < best_distance {
                    best_distance = distance;
                    best = index;
                }
            }
            response.on_hover_text(
                session.outputs[best]
                    .valid_time_utc
                    .format("%Y-%m-%d %H:%M:%SZ")
                    .to_string(),
            );
        }

        ui.label(kit::value_text(
            &last.valid_time_utc.format("%H:%MZ").to_string(),
        ));

        let following = session.selected_frame.is_none();
        let live_label = if session.terminal.is_none() {
            "LATEST"
        } else {
            "END"
        };
        if ui
            .add(egui::Button::selectable(following, live_label))
            .on_hover_text("Follow the newest committed frame")
            .clicked()
        {
            actions.select_frame = Some(None);
        }
    });
}
