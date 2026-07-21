//! Hosting chrome for the vertical cross-section viewer.
//!
//! The section renderer and its map-owned sampling state stay on
//! [`ViewerApp`]. This module only chooses between the historical resizable
//! panel below the map and a resizable floating `egui::Window`, so moving the
//! viewer never rebuilds or copies the section.

use eframe::egui;

use crate::ViewerApp;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CrossSectionSurface {
    Hidden,
    Bottom,
    Floating,
}

pub(super) fn resolve_cross_section_surface(
    open: bool,
    floating_preference: bool,
) -> CrossSectionSurface {
    match (open, floating_preference) {
        (false, _) => CrossSectionSurface::Hidden,
        (true, false) => CrossSectionSurface::Bottom,
        (true, true) => CrossSectionSurface::Floating,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeaderAction {
    Float,
    DockBelow,
    Hide,
}

impl ViewerApp {
    pub(super) fn cross_section_surface(&self) -> CrossSectionSurface {
        resolve_cross_section_surface(
            self.cross_section_view_open,
            self.app_settings.cross_section_floating,
        )
    }

    fn set_cross_section_floating_preference(&mut self, floating: bool) {
        if self.app_settings.cross_section_floating == floating {
            return;
        }
        self.app_settings.cross_section_floating = floating;
        self.mark_app_settings_dirty();
    }

    pub(super) fn hide_cross_section_view(&mut self) {
        // A hidden drawing tool must not keep owning map clicks. Completed
        // A/B endpoints remain intact so Show XS can restore the same section;
        // an unfinished A endpoint follows the normal disarm cleanup.
        self.set_cross_section_armed(false);
        self.cross_section_view_open = false;
    }

    /// One transition shared by the sidebar checkbox and Shift+X. Turning the
    /// tool off removes only an unfinished A endpoint; a completed A/B slice
    /// remains available in the viewer.
    pub(super) fn set_cross_section_armed(&mut self, armed: bool) {
        let was_armed = self.cross_section_armed;
        self.cross_section_armed = armed;
        if armed {
            self.cross_section_view_open = true;
        } else if was_armed && self.cross_section_b_lonlat.is_none() {
            if self.cross_section_a_lonlat.is_some() {
                self.cross_section_a_lonlat = None;
                self.cross_section_signature = None;
                self.cross_section_status =
                    "Cross-section: arm, then click endpoint A then B".to_owned();
            }
            self.cross_section_view_open = false;
        }
    }

    pub(super) fn toggle_cross_section_armed(&mut self) {
        self.set_cross_section_armed(!self.cross_section_armed);
    }

    /// Shared title/status row for both hosts. Returns false when the caller
    /// should stop drawing the old host for this repaint.
    pub(super) fn cross_section_header_ui(
        &mut self,
        ui: &mut egui::Ui,
        surface: CrossSectionSurface,
    ) -> bool {
        debug_assert_ne!(surface, CrossSectionSurface::Hidden);
        let mut action = None;
        ui.horizontal(|ui| {
            ui.strong("Cross-Section");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("Hide")
                    .on_hover_text("Hide the viewer without clearing its A/B section")
                    .clicked()
                {
                    action = Some(HeaderAction::Hide);
                }
                match surface {
                    CrossSectionSurface::Bottom => {
                        if ui
                            .small_button("Float window")
                            .on_hover_text(
                                "Pop the cross-section into a resizable window above the map",
                            )
                            .clicked()
                        {
                            action = Some(HeaderAction::Float);
                        }
                    }
                    CrossSectionSurface::Floating => {
                        if ui
                            .small_button("Dock below map")
                            .on_hover_text("Return the cross-section to its resizable bottom panel")
                            .clicked()
                        {
                            action = Some(HeaderAction::DockBelow);
                        }
                    }
                    CrossSectionSurface::Hidden => {}
                }
            });
        });
        ui.weak(&self.cross_section_status);

        match action {
            Some(HeaderAction::Float) => {
                self.set_cross_section_floating_preference(true);
                ui.ctx().request_repaint();
                false
            }
            Some(HeaderAction::DockBelow) => {
                self.set_cross_section_floating_preference(false);
                ui.ctx().request_repaint();
                false
            }
            Some(HeaderAction::Hide) => {
                self.hide_cross_section_view();
                ui.ctx().request_repaint();
                false
            }
            None => true,
        }
    }

    pub(super) fn show_cross_section_floating_window(&mut self, ctx: &egui::Context) {
        if self.cross_section_surface() != CrossSectionSurface::Floating {
            return;
        }

        let bounds = ctx.content_rect().shrink(8.0);
        let min_size = egui::vec2(480.0, 260.0);
        let max_size = egui::vec2(
            bounds.width().max(min_size.x),
            bounds.height().max(min_size.y),
        );
        let default_size = egui::vec2(
            900.0_f32.min(max_size.x).max(min_size.x),
            430.0_f32.min(max_size.y).max(min_size.y),
        );
        let mut open = true;
        egui::Window::new("Cross-Section")
            .id(egui::Id::new("cross_section_floating_window"))
            .open(&mut open)
            .default_size(default_size)
            .min_size(min_size)
            .max_size(max_size)
            .constrain_to(bounds)
            .resizable(true)
            .show(ctx, |ui| {
                self.cross_section_panel(ui, CrossSectionSurface::Floating);
            });
        if !open {
            self.hide_cross_section_view();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_section_surface_is_exclusive_and_respects_host_preference() {
        assert_eq!(
            resolve_cross_section_surface(false, false),
            CrossSectionSurface::Hidden
        );
        assert_eq!(
            resolve_cross_section_surface(false, true),
            CrossSectionSurface::Hidden
        );
        assert_eq!(
            resolve_cross_section_surface(true, false),
            CrossSectionSurface::Bottom
        );
        assert_eq!(
            resolve_cross_section_surface(true, true),
            CrossSectionSurface::Floating
        );
    }
}
