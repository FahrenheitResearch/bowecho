// SPDX-License-Identifier: Apache-2.0

//! The render-product picker: the RENDERER's own catalog (via
//! `run-plan --catalog`), searchable, with favorites (★) and an explicit
//! custom list — both submitted VERBATIM as the prepared route's
//! `render_products` run option. Studio adds display metadata only
//! (color_tables::wrf_fields descriptions where a name matches); the
//! list itself is never Studio's.
//!
//! Honesty rules carried here: the answering engine (rust vs
//! matplotlib) is shown; a `parse_warning` from the engine is surfaced
//! in alert color, never swallowed; group keywords are offered as the
//! engine's own tokens with the engine's own spec sentence on hover.

use eframe::egui;

use arwen_plan::queries::CatalogReport;

use crate::draft::{Draft, RenderMode};
use crate::theme::theme;

#[derive(Default)]
pub struct ProductsState {
    pub open: bool,
    pub search: String,
    pub catalog: Option<Result<CatalogReport, String>>,
    pub fetching: bool,
}

#[derive(Debug, Default)]
pub struct ProductsActions {
    /// Favorites changed — persist settings.
    pub favorites_changed: bool,
}

/// Toggle membership; returns true when toggled in.
fn toggle(list: &mut Vec<String>, name: &str) -> bool {
    if let Some(position) = list.iter().position(|item| item == name) {
        list.remove(position);
        false
    } else {
        list.push(name.to_string());
        true
    }
}

pub fn window_ui(
    ctx: &egui::Context,
    state: &mut ProductsState,
    draft: &mut Draft,
    favorites: &mut Vec<String>,
    actions: &mut ProductsActions,
) {
    let mut open = state.open;
    egui::Window::new("Render products — the renderer's own catalog")
        .open(&mut open)
        .resizable(true)
        .default_width(460.0)
        .default_height(560.0)
        .show(ctx, |ui| {
            match (&state.catalog, state.fetching) {
                (None, true) => {
                    ui.colored_label(theme().text_weak, "asking the engine for the catalog…");
                    return;
                }
                (None, false) => {
                    ui.colored_label(theme().text_weak, "catalog not fetched yet");
                    return;
                }
                (Some(Err(error)), _) => {
                    ui.colored_label(theme().alert, format!("catalog failed: {error}"));
                    return;
                }
                (Some(Ok(_)), _) => {}
            }
            let catalog = match &state.catalog {
                Some(Ok(catalog)) => catalog.clone(),
                _ => return,
            };

            // Provenance + engine badge, and the MUST-surface warning.
            let engine = catalog.engine.as_deref().unwrap_or("?");
            let engine_color = if engine == "rust" {
                theme().live
            } else {
                theme().warn
            };
            ui.horizontal(|ui| {
                ui.colored_label(engine_color, format!("catalog engine: {engine}"))
                    .on_hover_text(
                        catalog
                            .source
                            .clone()
                            .unwrap_or_else(|| "engine-relayed".into()),
                    );
                ui.label(
                    egui::RichText::new(format!("{} products", catalog.products.len()))
                        .color(theme().text_weak),
                );
            });
            if let Some(warning) = &catalog.parse_warning {
                ui.colored_label(theme().alert, format!("engine parse warning: {warning}"));
            }
            if let Some(notice) = &catalog.engine_notice {
                ui.colored_label(theme().warn, notice);
            }

            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.search)
                        .hint_text("Search products…")
                        .desired_width(200.0),
                );
                if ui
                    .button("Clear list")
                    .on_hover_text("Empty the custom selection")
                    .clicked()
                {
                    draft.render_custom.clear();
                }
            });

            // The engine's own group tokens, submitable verbatim.
            let spec_hover = catalog
                .spec
                .clone()
                .unwrap_or_else(|| "engine token".into());
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new("group tokens:")
                        .color(theme().text_weak)
                        .size(10.5),
                );
                for keyword in &catalog.group_keywords {
                    if keyword == "all" {
                        continue; // "All" is a mode chip on the card
                    }
                    let selected = draft.render_custom.iter().any(|item| item == keyword);
                    if ui
                        .add(egui::Button::selectable(selected, keyword))
                        .on_hover_text(format!(
                            "engine group token, submitted verbatim — {spec_hover}"
                        ))
                        .clicked()
                    {
                        toggle(&mut draft.render_custom, keyword);
                        draft.render_mode = RenderMode::Custom;
                    }
                }
            });
            ui.separator();

            let search = state.search.to_lowercase();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for product in &catalog.products {
                    let name = &product.name;
                    // Display metadata only: a wrf_fields description
                    // when the name matches a known store field.
                    let description = color_tables::wrf_fields::wrf_field_info(name)
                        .map(|info| info.description)
                        .unwrap_or("");
                    if !search.is_empty()
                        && !name.to_lowercase().contains(&search)
                        && !description.to_lowercase().contains(&search)
                    {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        let selected = draft.render_custom.iter().any(|item| item == name);
                        let mut checked = selected;
                        if ui
                            .checkbox(&mut checked, "")
                            .on_hover_text("In the explicit render list (run option, verbatim)")
                            .changed()
                        {
                            toggle(&mut draft.render_custom, name);
                            draft.render_mode = RenderMode::Custom;
                        }
                        let starred = favorites.iter().any(|item| item == name);
                        let star = if starred { "★" } else { "☆" };
                        if ui
                            .add(egui::Button::new(star).frame(false))
                            .on_hover_text("Favorite — the Favorites render mode uses these")
                            .clicked()
                        {
                            toggle(favorites, name);
                            actions.favorites_changed = true;
                        }
                        let mut text = egui::RichText::new(name).monospace().size(11.0);
                        if selected {
                            text = text.color(theme().accent_text);
                        }
                        let label = ui.label(text);
                        if !description.is_empty() {
                            label.on_hover_text(description);
                        }
                    });
                }
            });
        });
    state.open = open;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_round_trips() {
        let mut list = Vec::new();
        assert!(toggle(&mut list, "sbcape"));
        assert_eq!(list, vec!["sbcape".to_string()]);
        assert!(!toggle(&mut list, "sbcape"));
        assert!(list.is_empty());
    }

    #[test]
    fn fixture_catalog_parses_with_products_and_tokens() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/catalog.json");
        let text = std::fs::read_to_string(path).unwrap();
        let catalog: CatalogReport = serde_json::from_str(&text).unwrap();
        assert_eq!(catalog.schema, "gpuwm.run-plan.catalog.v1");
        assert_eq!(catalog.products.len(), 150);
        assert!(catalog.group_keywords.contains(&"windowed".to_string()));
        assert_eq!(catalog.skip_token.as_deref(), Some("none"));
        assert_eq!(catalog.engine.as_deref(), Some("rust"));
    }
}
