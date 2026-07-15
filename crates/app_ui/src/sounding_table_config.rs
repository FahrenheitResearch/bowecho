//! Persistent, user-editable configuration for the three scalar tables in
//! the native SHARPpy sounding board.
//!
//! The renderer deliberately lives elsewhere. This module owns stable
//! selector identities, tolerant persistence, and the egui editor. Unknown
//! built-in or Formula Lab IDs survive round trips so a temporarily missing
//! plugin/result never destroys a user's layout.

use std::collections::BTreeSet;

use eframe::egui;
use serde::{Deserialize, Serialize};

pub(crate) const SOUNDING_TABLE_VIEW_STATE_KEY: &str = "sharppy_table_board";
const SOUNDING_TABLE_SCHEMA: u8 = 1;
const MAX_SECTIONS_PER_PANEL: usize = 24;
const MAX_SLOTS_PER_SECTION: usize = 96;
const MAX_TEXT_CHARS: usize = 80;

/// The three independently titled, movable scalar-table panels already
/// present in the sounding layout.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SoundingTablePanelId {
    #[default]
    Convective,
    Kinematics,
    Severe,
}

impl SoundingTablePanelId {
    pub(crate) const ALL: [Self; 3] = [Self::Convective, Self::Kinematics, Self::Severe];

    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::Convective => "convective",
            Self::Kinematics => "kinematics",
            Self::Severe => "severe",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Convective => "Parcels & thermo",
            Self::Kinematics => "Kinematics",
            Self::Severe => "Severe indices",
        }
    }
}

/// A table row's value source. String IDs are intentional: built-ins can be
/// added without invalidating old binaries, and Formula Lab recipes can use a
/// content-derived stable ID. Empty/unknown IDs remain representable and are
/// surfaced as unavailable by the renderer/editor instead of being dropped.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub(crate) enum SoundingDiagnosticRef {
    BuiltIn { id: String },
    Formula { id: String },
    Blank,
}

impl Default for SoundingDiagnosticRef {
    fn default() -> Self {
        Self::Blank
    }
}

impl SoundingDiagnosticRef {
    pub(crate) fn built_in(id: impl Into<String>) -> Self {
        Self::BuiltIn { id: id.into() }
    }

    pub(crate) fn formula(id: impl Into<String>) -> Self {
        Self::Formula { id: id.into() }
    }

    pub(crate) fn stable_key(&self) -> String {
        match self {
            Self::BuiltIn { id } => format!("builtin:{id}"),
            Self::Formula { id } => format!("formula:{id}"),
            Self::Blank => "blank".to_owned(),
        }
    }

    pub(crate) fn is_blank(&self) -> bool {
        matches!(self, Self::Blank)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SoundingTableSlot {
    pub(crate) diagnostic: SoundingDiagnosticRef,
    /// Optional display-only label. `None` uses the catalog's canonical name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) label_override: Option<String>,
}

impl SoundingTableSlot {
    pub(crate) fn new(diagnostic: SoundingDiagnosticRef) -> Self {
        Self {
            diagnostic,
            label_override: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SoundingTableSection {
    /// Stable only within this configuration; used for egui identity.
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) slots: Vec<SoundingTableSlot>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SoundingTablePanelConfig {
    pub(crate) panel: SoundingTablePanelId,
    pub(crate) title: String,
    /// Whether this panel replaces sharppyrs' exact native table. Older
    /// saved custom boards predate this switch and therefore deserialize as
    /// active; freshly seeded editor templates explicitly start inactive.
    #[serde(default = "legacy_panel_override_active")]
    pub(crate) override_active: bool,
    #[serde(default)]
    pub(crate) sections: Vec<SoundingTableSection>,
}

const fn legacy_panel_override_active() -> bool {
    true
}

impl SoundingTablePanelConfig {
    pub(crate) fn is_override_active(&self) -> bool {
        self.override_active
    }

    fn activate_override(&mut self) {
        self.override_active = true;
    }

    fn deactivate_override(&mut self) {
        self.override_active = false;
    }
}

impl Default for SoundingTablePanelConfig {
    fn default() -> Self {
        Self {
            panel: SoundingTablePanelId::default(),
            title: SoundingTablePanelId::default().label().to_owned(),
            override_active: false,
            sections: Vec::new(),
        }
    }
}

/// Default means "no host override": sharppyrs draws its exact canonical
/// tables. A custom configuration contains only the three table panels; the
/// surrounding sounding geometry remains owned by `SoundingLayout`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SoundingTableConfig {
    schema: u8,
    pub(crate) custom: bool,
    #[serde(default)]
    pub(crate) panels: Vec<SoundingTablePanelConfig>,
    /// Opaque state written by a newer BowEcho. Preserve it until the user
    /// explicitly replaces or resets the board instead of deleting it during
    /// an automatic settings save from an older build.
    #[serde(skip)]
    preserved_future: Option<serde_json::Value>,
}

impl Default for SoundingTableConfig {
    fn default() -> Self {
        Self {
            schema: SOUNDING_TABLE_SCHEMA,
            custom: false,
            panels: Vec::new(),
            preserved_future: None,
        }
    }
}

impl SoundingTableConfig {
    /// Build a renderer/editor template while keeping the public default as
    /// the untouched canonical board.
    pub(crate) fn custom_template(panels: Vec<SoundingTablePanelConfig>) -> Self {
        let mut config = Self {
            schema: SOUNDING_TABLE_SCHEMA,
            custom: true,
            panels,
            preserved_future: None,
        };
        for panel in &mut config.panels {
            panel.override_active = false;
        }
        config.normalize();
        config
    }

    pub(crate) fn is_custom(&self) -> bool {
        self.custom
    }

    fn has_preserved_future(&self) -> bool {
        self.preserved_future.is_some()
    }

    pub(crate) fn panel(&self, panel: SoundingTablePanelId) -> Option<&SoundingTablePanelConfig> {
        self.panels
            .iter()
            .find(|candidate| candidate.panel == panel)
    }

    pub(crate) fn panel_mut(
        &mut self,
        panel: SoundingTablePanelId,
    ) -> Option<&mut SoundingTablePanelConfig> {
        self.panels
            .iter_mut()
            .find(|candidate| candidate.panel == panel)
    }

    /// True only when this individual panel should replace the native
    /// sharppyrs renderer. Merely opening the editor never enables it.
    pub(crate) fn panel_override_active(&self, panel: SoundingTablePanelId) -> bool {
        self.panel(panel)
            .is_some_and(SoundingTablePanelConfig::is_override_active)
    }

    pub(crate) fn start_customizing(&mut self, defaults: &Self) {
        *self = defaults.clone();
        self.schema = SOUNDING_TABLE_SCHEMA;
        self.custom = true;
        for panel in &mut self.panels {
            panel.override_active = false;
        }
        self.normalize();
    }

    pub(crate) fn reset_to_canonical(&mut self) {
        *self = Self::default();
    }

    /// Bound hostile/corrupt state without discarding forward-compatible
    /// diagnostic IDs. Duplicate panels are collapsed deterministically.
    pub(crate) fn normalize(&mut self) {
        self.schema = SOUNDING_TABLE_SCHEMA;
        if self.custom {
            self.preserved_future = None;
        }
        let mut seen_panels = BTreeSet::new();
        self.panels.retain(|panel| seen_panels.insert(panel.panel));
        for panel in &mut self.panels {
            panel.title = bounded_text(&panel.title);
            if panel.title.is_empty() {
                panel.title = panel.panel.label().to_owned();
            }
            panel.sections.truncate(MAX_SECTIONS_PER_PANEL);
            let mut seen_ids = BTreeSet::new();
            for (index, section) in panel.sections.iter_mut().enumerate() {
                section.id = bounded_text(&section.id);
                if section.id.is_empty() || !seen_ids.insert(section.id.clone()) {
                    section.id = unique_section_id(panel.panel, index, &mut seen_ids);
                }
                section.title = bounded_text(&section.title);
                section.slots.truncate(MAX_SLOTS_PER_SECTION);
                for slot in &mut section.slots {
                    if let Some(label) = slot.label_override.as_mut() {
                        *label = bounded_text(label);
                        if label.is_empty() {
                            slot.label_override = None;
                        }
                    }
                }
            }
        }
    }

    /// Bound live editor state without stripping a trailing space while the
    /// user is in the middle of typing a multi-word title or label.
    fn normalize_live(&mut self) {
        self.schema = SOUNDING_TABLE_SCHEMA;
        self.preserved_future = None;
        let mut seen_panels = BTreeSet::new();
        self.panels.retain(|panel| seen_panels.insert(panel.panel));
        for panel in &mut self.panels {
            panel.title = bounded_live_text(&panel.title);
            if panel.title.trim().is_empty() {
                panel.title = panel.panel.label().to_owned();
            }
            panel.sections.truncate(MAX_SECTIONS_PER_PANEL);
            let mut seen_ids = BTreeSet::new();
            for (index, section) in panel.sections.iter_mut().enumerate() {
                section.id = bounded_text(&section.id);
                if section.id.is_empty() || !seen_ids.insert(section.id.clone()) {
                    section.id = unique_section_id(panel.panel, index, &mut seen_ids);
                }
                section.title = bounded_live_text(&section.title);
                section.slots.truncate(MAX_SLOTS_PER_SECTION);
                for slot in &mut section.slots {
                    if let Some(label) = slot.label_override.as_mut() {
                        *label = bounded_live_text(label);
                        if label.trim().is_empty() {
                            slot.label_override = None;
                        }
                    }
                }
            }
        }
    }
}

fn bounded_text(value: &str) -> String {
    value.trim().chars().take(MAX_TEXT_CHARS).collect()
}

fn bounded_live_text(value: &str) -> String {
    value.chars().take(MAX_TEXT_CHARS).collect()
}

fn unique_section_id(
    panel: SoundingTablePanelId,
    seed: usize,
    seen: &mut BTreeSet<String>,
) -> String {
    for suffix in seed.. {
        let candidate = format!("{}-section-{}", panel.token(), suffix + 1);
        if seen.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("an unbounded integer sequence always contains a free section id")
}

/// One selectable metric supplied by the renderer/evaluator registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SoundingDiagnosticOption {
    pub(crate) diagnostic: SoundingDiagnosticRef,
    pub(crate) label: String,
    pub(crate) category: String,
    pub(crate) unit: Option<String>,
    pub(crate) description: String,
}

impl SoundingDiagnosticOption {
    pub(crate) fn built_in(
        id: impl Into<String>,
        label: impl Into<String>,
        category: impl Into<String>,
        unit: Option<impl Into<String>>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            diagnostic: SoundingDiagnosticRef::built_in(id),
            label: label.into(),
            category: category.into(),
            unit: unit.map(Into::into),
            description: description.into(),
        }
    }

    pub(crate) fn formula(
        id: impl Into<String>,
        label: impl Into<String>,
        unit: Option<impl Into<String>>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            diagnostic: SoundingDiagnosticRef::formula(id),
            label: label.into(),
            category: "Formula Lab".to_owned(),
            unit: unit.map(Into::into),
            description: description.into(),
        }
    }
}

pub(crate) fn diagnostic_option<'a>(
    catalog: &'a [SoundingDiagnosticOption],
    diagnostic: &SoundingDiagnosticRef,
) -> Option<&'a SoundingDiagnosticOption> {
    catalog
        .iter()
        .find(|option| option.diagnostic == *diagnostic)
}

pub(crate) fn diagnostic_display_name(
    catalog: &[SoundingDiagnosticOption],
    diagnostic: &SoundingDiagnosticRef,
) -> String {
    if diagnostic.is_blank() {
        return "(blank)".to_owned();
    }
    diagnostic_option(catalog, diagnostic)
        .map(|option| option.label.clone())
        .unwrap_or_else(|| format!("Unavailable · {}", diagnostic.stable_key()))
}

/// Restore a custom table board from the sounding's opaque view state.
/// Missing or malformed state safely selects the canonical board. A future
/// schema renders canonically in this older binary but is retained opaquely;
/// unknown diagnostic IDs inside a supported schema are also retained.
pub(crate) fn config_from_view_state(value: &serde_json::Value) -> Option<SoundingTableConfig> {
    let encoded = value.get(SOUNDING_TABLE_VIEW_STATE_KEY)?;
    if encoded
        .get("schema")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|schema| schema > u64::from(SOUNDING_TABLE_SCHEMA))
    {
        return Some(SoundingTableConfig {
            preserved_future: Some(encoded.clone()),
            ..SoundingTableConfig::default()
        });
    }
    let mut config = serde_json::from_value::<SoundingTableConfig>(encoded.clone()).ok()?;
    if config.schema != SOUNDING_TABLE_SCHEMA {
        return None;
    }
    config.normalize();
    config.custom.then_some(config)
}

/// Merge table configuration into the existing classic-panel view-state
/// object. Canonical/default state removes only our own key.
pub(crate) fn write_config_to_view_state(
    value: &mut serde_json::Value,
    config: &SoundingTableConfig,
) -> bool {
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    if !config.custom {
        if let Some(encoded) = &config.preserved_future {
            object.insert(SOUNDING_TABLE_VIEW_STATE_KEY.to_owned(), encoded.clone());
        } else {
            object.remove(SOUNDING_TABLE_VIEW_STATE_KEY);
        }
        return true;
    }
    let mut normalized = config.clone();
    normalized.normalize();
    let Ok(encoded) = serde_json::to_value(normalized) else {
        return false;
    };
    object.insert(SOUNDING_TABLE_VIEW_STATE_KEY.to_owned(), encoded);
    true
}

/// Local UI state is intentionally not persisted: closing/reopening the
/// editor returns to a clean search while the board itself remains saved.
#[derive(Debug, Default)]
pub(crate) struct SoundingTableEditor {
    open: bool,
    selected_panel: SoundingTablePanelId,
    search: String,
    collapsed_panels: BTreeSet<SoundingTablePanelId>,
    collapsed_sections: BTreeSet<(SoundingTablePanelId, String)>,
}

impl SoundingTableEditor {
    pub(crate) fn header_button(
        &mut self,
        ui: &mut egui::Ui,
        config: &SoundingTableConfig,
    ) -> bool {
        let label = if config.custom {
            "Tables · Custom"
        } else {
            "Tables"
        };
        let clicked = ui
            .add(egui::Button::selectable(self.open, label))
            .on_hover_text(
                "Choose, reorder, rename, or hide every scalar sounding-table row, including compatible Formula Lab diagnostics.",
            )
            .clicked();
        if clicked {
            self.open = !self.open;
        }
        clicked
    }

    pub(crate) fn show(
        &mut self,
        ctx: &egui::Context,
        config: &mut SoundingTableConfig,
        defaults: &SoundingTableConfig,
        catalog: &[SoundingDiagnosticOption],
    ) -> bool {
        if !self.open {
            return false;
        }
        let mut open = self.open;
        let mut changed = false;
        egui::Window::new("Sounding table editor")
            .id(egui::Id::new("bowecho_sounding_table_editor"))
            .open(&mut open)
            .default_size(egui::vec2(760.0, 620.0))
            .min_size(egui::vec2(560.0, 400.0))
            .resizable(true)
            .show(ctx, |ui| {
                changed |= Self::editor_body(
                    ui,
                    &mut self.selected_panel,
                    &mut self.search,
                    &mut self.collapsed_panels,
                    &mut self.collapsed_sections,
                    config,
                    defaults,
                    catalog,
                );
            });
        self.open = open;
        if changed {
            config.normalize_live();
        }
        changed
    }

    fn editor_body(
        ui: &mut egui::Ui,
        selected_panel: &mut SoundingTablePanelId,
        search: &mut String,
        collapsed_panels: &mut BTreeSet<SoundingTablePanelId>,
        collapsed_sections: &mut BTreeSet<(SoundingTablePanelId, String)>,
        config: &mut SoundingTableConfig,
        defaults: &SoundingTableConfig,
        catalog: &[SoundingDiagnosticOption],
    ) -> bool {
        let mut changed = false;
        ui.heading("Diagnostic tables");
        ui.label(
            "Every row is independent. Pick any supported diagnostic, a compatible Formula Lab result, or blank; duplicate values are allowed.",
        );
        ui.add_space(4.0);
        if !config.custom {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                if config.has_preserved_future() {
                    ui.label(
                        egui::RichText::new("Newer table layout preserved")
                            .strong()
                            .color(egui::Color32::from_rgb(255, 190, 70)),
                    );
                    ui.weak(
                        "This layout was written by a newer BowEcho and will remain unchanged unless you replace it here.",
                    );
                } else {
                    ui.label(egui::RichText::new("Canonical SHARPpy tables").strong());
                    ui.weak("No override is active, so the built-in table content is unchanged.");
                }
                if ui.button("Customize tables").clicked() {
                    config.start_customizing(defaults);
                    changed = true;
                }
            });
            return changed;
        }

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("CUSTOM")
                    .small()
                    .strong()
                    .color(egui::Color32::from_rgb(255, 190, 70)),
            );
            ui.weak("Changes preview immediately and save with the sounding view.");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("Reset all to default")
                    .on_hover_text("Remove the override and restore the exact built-in tables")
                    .clicked()
                {
                    config.reset_to_canonical();
                    changed = true;
                }
            });
        });
        if !config.custom {
            return changed;
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            for panel in SoundingTablePanelId::ALL {
                let collapsed = collapsed_panels.contains(&panel);
                if ui
                    .add(egui::Button::new(if collapsed { "\u{25b8}" } else { "\u{25be}" }).small())
                    .on_hover_text(if collapsed {
                        "Expand this panel editor"
                    } else {
                        "Collapse this panel editor"
                    })
                    .clicked()
                {
                    *selected_panel = panel;
                    if collapsed {
                        collapsed_panels.remove(&panel);
                    } else {
                        collapsed_panels.insert(panel);
                    }
                }
                if ui
                    .selectable_label(*selected_panel == panel, panel.label())
                    .clicked()
                {
                    *selected_panel = panel;
                    collapsed_panels.remove(&panel);
                }
            }
        });
        ui.add_space(4.0);

        let panel_id = *selected_panel;
        if config.panel(panel_id).is_none() {
            let replacement =
                defaults
                    .panel(panel_id)
                    .cloned()
                    .unwrap_or_else(|| SoundingTablePanelConfig {
                        panel: panel_id,
                        title: panel_id.label().to_owned(),
                        override_active: false,
                        sections: Vec::new(),
                    });
            config.panels.push(replacement);
            changed = true;
        }
        if collapsed_panels.contains(&panel_id) {
            ui.weak("Panel editor collapsed. The sounding table is unchanged.");
            return changed;
        }
        let default_panel = defaults.panel(panel_id).cloned();
        let panel = config
            .panel_mut(panel_id)
            .expect("the selected table panel was inserted above");

        let mut panel_edited = false;
        let mut restored_panel = false;
        ui.horizontal(|ui| {
            ui.label("Panel title");
            panel_edited |= ui
                .add(egui::TextEdit::singleline(&mut panel.title).desired_width(260.0))
                .changed();
            if ui.small_button("Restore panel").clicked()
                && let Some(default_panel) = default_panel.clone()
            {
                *panel = default_panel;
                panel.deactivate_override();
                changed = true;
                panel_edited = false;
                restored_panel = true;
            }
            if panel.is_override_active() {
                ui.label(
                    egui::RichText::new("OVERRIDE ACTIVE")
                        .small()
                        .strong()
                        .color(egui::Color32::from_rgb(255, 190, 70)),
                );
            } else {
                ui.weak("Native table remains active until you edit this panel");
            }
        });
        ui.horizontal(|ui| {
            ui.label("Find diagnostic");
            ui.add(
                egui::TextEdit::singleline(search)
                    .hint_text("CAPE, shear, PWAT, Formula Lab…")
                    .desired_width(300.0),
            );
            if !search.is_empty() && ui.small_button("Clear").clicked() {
                search.clear();
            }
        });
        ui.separator();

        let mut section_move = None;
        let mut section_remove = None;
        egui::ScrollArea::vertical()
            .id_salt(("sounding_table_sections", panel_id.token()))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let section_count = panel.sections.len();
                for section_index in 0..section_count {
                    let section = &mut panel.sections[section_index];
                    let collapse_key = (panel_id, section.id.clone());
                    let mut section_open = !collapsed_sections.contains(&collapse_key);
                    egui::Frame::group(ui.style())
                        .inner_margin(egui::Margin::same(8))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if ui
                                    .add(
                                        egui::Button::new(if section_open {
                                            "\u{25be}"
                                        } else {
                                            "\u{25b8}"
                                        })
                                        .small(),
                                    )
                                    .on_hover_text(if section_open {
                                        "Collapse this section"
                                    } else {
                                        "Expand this section"
                                    })
                                    .clicked()
                                {
                                    section_open = !section_open;
                                    if section_open {
                                        collapsed_sections.remove(&collapse_key);
                                    } else {
                                        collapsed_sections.insert(collapse_key.clone());
                                    }
                                }
                                ui.label("Section");
                                panel_edited |= ui
                                    .add(
                                        egui::TextEdit::singleline(&mut section.title)
                                            .desired_width(220.0),
                                    )
                                    .changed();
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("Remove").clicked() {
                                            section_remove = Some(section_index);
                                        }
                                        if ui
                                            .add_enabled(
                                                section_index + 1 < section_count,
                                                egui::Button::new("↓"),
                                            )
                                            .on_hover_text("Move section down")
                                            .clicked()
                                        {
                                            section_move = Some((section_index, section_index + 1));
                                        }
                                        if ui
                                            .add_enabled(
                                                section_index > 0,
                                                egui::Button::new("↑"),
                                            )
                                            .on_hover_text("Move section up")
                                            .clicked()
                                        {
                                            section_move = Some((section_index, section_index - 1));
                                        }
                                    },
                                );
                            });

                            if section_open {
                            let mut slot_move = None;
                            let mut slot_remove = None;
                            let mut slot_duplicate = None;
                            let slot_count = section.slots.len();
                            for slot_index in 0..slot_count {
                                let slot = &mut section.slots[slot_index];
                                ui.push_id((&section.id, slot_index), |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!("{:02}", slot_index + 1))
                                                .monospace()
                                                .weak(),
                                        );
                                        if ui
                                            .add_enabled(
                                                slot_index > 0,
                                                egui::Button::new("↑").small(),
                                            )
                                            .on_hover_text("Move row up")
                                            .clicked()
                                        {
                                            slot_move = Some((slot_index, slot_index - 1));
                                        }
                                        if ui
                                            .add_enabled(
                                                slot_index + 1 < slot_count,
                                                egui::Button::new("↓").small(),
                                            )
                                            .on_hover_text("Move row down")
                                            .clicked()
                                        {
                                            slot_move = Some((slot_index, slot_index + 1));
                                        }
                                        panel_edited |= diagnostic_picker(
                                            ui,
                                            slot,
                                            search,
                                            catalog,
                                            panel_id,
                                            &section.id,
                                            slot_index,
                                        );
                                        let mut override_text =
                                            slot.label_override.clone().unwrap_or_default();
                                        if ui
                                            .add(
                                                egui::TextEdit::singleline(&mut override_text)
                                                    .hint_text("automatic label")
                                                    .desired_width(130.0),
                                            )
                                            .on_hover_text(
                                                "Optional display label; leave empty for the diagnostic's standard name",
                                            )
                                            .changed()
                                        {
                                            slot.label_override = if override_text.trim().is_empty() {
                                                None
                                            } else {
                                                Some(override_text)
                                            };
                                            panel_edited = true;
                                        }
                                        if ui
                                            .small_button("Blank")
                                            .on_hover_text("Keep this row as intentional empty spacing")
                                            .clicked()
                                        {
                                            slot.diagnostic = SoundingDiagnosticRef::Blank;
                                            panel_edited = true;
                                        }
                                        if ui
                                            .small_button("Copy")
                                            .on_hover_text(
                                                "Duplicate this diagnostic and its custom label directly below",
                                            )
                                            .clicked()
                                        {
                                            slot_duplicate = Some(slot_index);
                                        }
                                        if ui
                                            .small_button("×")
                                            .on_hover_text("Remove this row and free its space")
                                            .clicked()
                                        {
                                            slot_remove = Some(slot_index);
                                        }
                                    });
                                });
                            }
                            if let Some((from, to)) = slot_move {
                                section.slots.swap(from, to);
                                panel_edited = true;
                            }
                            if let Some(index) = slot_remove {
                                section.slots.remove(index);
                                panel_edited = true;
                            } else if let Some(index) = slot_duplicate
                                && section.slots.len() < MAX_SLOTS_PER_SECTION
                            {
                                let duplicate = section.slots[index].clone();
                                section.slots.insert(index + 1, duplicate);
                                panel_edited = true;
                            }
                            if ui.small_button("+ Row").clicked() {
                                section.slots.push(SoundingTableSlot::default());
                                panel_edited = true;
                            }
                            }
                        });
                    ui.add_space(6.0);
                }
            });
        if let Some((from, to)) = section_move {
            panel.sections.swap(from, to);
            panel_edited = true;
        }
        if let Some(index) = section_remove {
            panel.sections.remove(index);
            panel_edited = true;
        }
        if ui.button("+ Section").clicked() {
            let mut seen = panel
                .sections
                .iter()
                .map(|section| section.id.clone())
                .collect::<BTreeSet<_>>();
            let id = unique_section_id(panel_id, panel.sections.len(), &mut seen);
            panel.sections.push(SoundingTableSection {
                id,
                title: format!("Section {}", panel.sections.len() + 1),
                slots: vec![SoundingTableSlot::default()],
            });
            panel_edited = true;
        }
        if panel_edited && !restored_panel {
            panel.activate_override();
            changed = true;
        }
        changed
    }
}

#[allow(clippy::too_many_arguments)]
fn diagnostic_picker(
    ui: &mut egui::Ui,
    slot: &mut SoundingTableSlot,
    search: &str,
    catalog: &[SoundingDiagnosticOption],
    panel: SoundingTablePanelId,
    section_id: &str,
    slot_index: usize,
) -> bool {
    let before = slot.diagnostic.clone();
    let selected = diagnostic_display_name(catalog, &slot.diagnostic);
    egui::ComboBox::from_id_salt(("sounding-diagnostic", panel.token(), section_id, slot_index))
        .selected_text(selected)
        .width(230.0)
        // Categories are collapsible controls inside this popup. egui's
        // ComboBox default is CloseOnClick, which would close the picker as
        // soon as the user tried to expand a category. Keep ordinary clicks
        // inside alive and explicitly close only after a real selection.
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show_ui(ui, |ui| {
            ui.set_min_width(320.0);
            if ui
                .selectable_value(
                    &mut slot.diagnostic,
                    SoundingDiagnosticRef::Blank,
                    "(blank)",
                )
                .clicked()
            {
                ui.close();
            }
            let needle = search.trim().to_ascii_lowercase();
            let mut categories = catalog
                .iter()
                .filter(|option| {
                    needle.is_empty()
                        || option.label.to_ascii_lowercase().contains(&needle)
                        || option.category.to_ascii_lowercase().contains(&needle)
                        || option
                            .diagnostic
                            .stable_key()
                            .to_ascii_lowercase()
                            .contains(&needle)
                })
                .map(|option| option.category.as_str())
                .collect::<Vec<_>>();
            categories.sort_unstable();
            categories.dedup();
            if categories.is_empty() {
                ui.weak("No diagnostics match the current search.");
            }
            for category in categories {
                ui.collapsing(category, |ui| {
                    for option in catalog.iter().filter(|option| {
                        option.category == category
                            && (needle.is_empty()
                                || option.label.to_ascii_lowercase().contains(&needle)
                                || option.category.to_ascii_lowercase().contains(&needle)
                                || option
                                    .diagnostic
                                    .stable_key()
                                    .to_ascii_lowercase()
                                    .contains(&needle))
                    }) {
                        let label = match option.unit.as_deref() {
                            Some(unit) if !unit.is_empty() => {
                                format!("{} · {}", option.label, unit)
                            }
                            _ => option.label.clone(),
                        };
                        if ui
                            .selectable_value(
                                &mut slot.diagnostic,
                                option.diagnostic.clone(),
                                label,
                            )
                            .on_hover_text(&option.description)
                            .clicked()
                        {
                            ui.close();
                        }
                    }
                });
            }
        });
    before != slot.diagnostic
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> SoundingTableConfig {
        SoundingTableConfig::custom_template(vec![
            SoundingTablePanelConfig {
                panel: SoundingTablePanelId::Convective,
                title: "Parcels & thermo".to_owned(),
                override_active: false,
                sections: vec![SoundingTableSection {
                    id: "parcel".to_owned(),
                    title: "Parcel".to_owned(),
                    slots: vec![SoundingTableSlot::new(SoundingDiagnosticRef::built_in(
                        "mu_cape",
                    ))],
                }],
            },
            SoundingTablePanelConfig {
                panel: SoundingTablePanelId::Kinematics,
                title: "Kinematics".to_owned(),
                override_active: false,
                sections: Vec::new(),
            },
            SoundingTablePanelConfig {
                panel: SoundingTablePanelId::Severe,
                title: "Severe indices".to_owned(),
                override_active: false,
                sections: Vec::new(),
            },
        ])
    }

    #[test]
    fn canonical_default_writes_no_override() {
        let mut state = serde_json::json!({"zooms": {"scene": 0.7}});
        assert!(write_config_to_view_state(
            &mut state,
            &SoundingTableConfig::default()
        ));
        assert!(state.get(SOUNDING_TABLE_VIEW_STATE_KEY).is_none());
        assert_eq!(state["zooms"]["scene"].as_f64(), Some(0.7));
    }

    #[test]
    fn entering_customization_keeps_every_native_panel_active() {
        let templates = defaults();
        let mut config = SoundingTableConfig::default();
        config.start_customizing(&templates);

        assert!(config.is_custom());
        for panel in SoundingTablePanelId::ALL {
            assert!(!config.panel_override_active(panel));
        }
    }

    #[test]
    fn first_content_edit_activates_only_that_panel_and_restore_deactivates_it() {
        let templates = defaults();
        let mut config = SoundingTableConfig::default();
        config.start_customizing(&templates);

        let kinematics = config
            .panel_mut(SoundingTablePanelId::Kinematics)
            .expect("kinematics template");
        kinematics.title = "My kinematics".to_owned();
        kinematics.activate_override();

        assert!(config.panel_override_active(SoundingTablePanelId::Kinematics));
        assert!(!config.panel_override_active(SoundingTablePanelId::Convective));
        assert!(!config.panel_override_active(SoundingTablePanelId::Severe));

        let mut restored = templates
            .panel(SoundingTablePanelId::Kinematics)
            .cloned()
            .expect("default kinematics template");
        restored.deactivate_override();
        *config
            .panel_mut(SoundingTablePanelId::Kinematics)
            .expect("kinematics template") = restored;
        assert!(!config.panel_override_active(SoundingTablePanelId::Kinematics));
    }

    #[test]
    fn custom_slots_sections_and_formula_ids_round_trip() {
        let mut config = defaults();
        let panel = config.panel_mut(SoundingTablePanelId::Convective).unwrap();
        panel.sections[0].slots.extend([
            SoundingTableSlot::new(SoundingDiagnosticRef::formula("recipe-sha256:1234")),
            SoundingTableSlot::default(),
        ]);
        panel.sections[0].slots[0].label_override = Some("My MU CAPE".to_owned());
        let mut state = serde_json::json!({"classic": true});
        assert!(write_config_to_view_state(&mut state, &config));
        let restored = config_from_view_state(&state).unwrap();
        assert_eq!(restored, config);
        assert_eq!(state["classic"].as_bool(), Some(true));
    }

    #[test]
    fn unknown_ids_survive_upgrades_instead_of_being_deleted() {
        let state = serde_json::json!({
            SOUNDING_TABLE_VIEW_STATE_KEY: {
                "schema": 1,
                "custom": true,
                "panels": [{
                    "panel": "severe",
                    "title": "Future",
                    "sections": [{
                        "id": "future",
                        "title": "Future values",
                        "slots": [
                            {"diagnostic": {"source": "built_in", "id": "future_builtin"}},
                            {"diagnostic": {"source": "formula", "id": "missing_formula"}}
                        ]
                    }]
                }]
            }
        });
        let restored = config_from_view_state(&state).unwrap();
        assert!(restored.panels[0].is_override_active());
        let slots = &restored.panels[0].sections[0].slots;
        assert_eq!(
            slots[0].diagnostic,
            SoundingDiagnosticRef::built_in("future_builtin")
        );
        assert_eq!(
            slots[1].diagnostic,
            SoundingDiagnosticRef::formula("missing_formula")
        );
        assert!(diagnostic_display_name(&[], &slots[0].diagnostic).starts_with("Unavailable"));
    }

    #[test]
    fn future_schema_is_preserved_until_explicitly_replaced() {
        let future = serde_json::json!({
            "schema": 99,
            "custom": true,
            "panels": [],
            "future_setting": {"mode": "new"}
        });
        let config = config_from_view_state(&serde_json::json!({
            SOUNDING_TABLE_VIEW_STATE_KEY: future.clone()
        }))
        .expect("future state is retained opaquely");
        assert!(!config.is_custom());
        assert!(config.has_preserved_future());
        let mut round_trip = serde_json::json!({});
        assert!(write_config_to_view_state(&mut round_trip, &config));
        assert_eq!(round_trip[SOUNDING_TABLE_VIEW_STATE_KEY], future);

        let mut replaced = config;
        replaced.start_customizing(&defaults());
        assert!(replaced.is_custom());
        assert!(!replaced.has_preserved_future());
    }

    #[test]
    fn malformed_schema_falls_back_to_canonical() {
        assert!(
            config_from_view_state(&serde_json::json!({
                SOUNDING_TABLE_VIEW_STATE_KEY: "not an object"
            }))
            .is_none()
        );
        assert!(config_from_view_state(&serde_json::json!({})).is_none());
    }

    #[test]
    fn normalization_bounds_text_and_deduplicates_panels_and_section_ids() {
        let duplicate = SoundingTablePanelConfig {
            panel: SoundingTablePanelId::Convective,
            title: " Duplicate ".to_owned(),
            override_active: true,
            sections: Vec::new(),
        };
        let mut config = defaults();
        config.panels.push(duplicate);
        let panel = config.panel_mut(SoundingTablePanelId::Convective).unwrap();
        panel.sections.push(SoundingTableSection {
            id: "parcel".to_owned(),
            title: "  Other  ".to_owned(),
            slots: Vec::new(),
        });
        config.normalize();
        assert_eq!(
            config
                .panels
                .iter()
                .filter(|panel| panel.panel == SoundingTablePanelId::Convective)
                .count(),
            1
        );
        let panel = config.panel(SoundingTablePanelId::Convective).unwrap();
        assert_eq!(panel.sections[1].title, "Other");
        assert_ne!(panel.sections[0].id, panel.sections[1].id);
    }

    #[test]
    fn live_normalization_keeps_spaces_while_typing_multiword_text() {
        let mut config = defaults();
        let panel = config.panel_mut(SoundingTablePanelId::Convective).unwrap();
        panel.title = "Storm ".to_owned();
        panel.sections[0].title = "Parcel ".to_owned();
        panel.sections[0].slots[0].label_override = Some("My ".to_owned());
        config.normalize_live();
        let panel = config.panel(SoundingTablePanelId::Convective).unwrap();
        assert_eq!(panel.title, "Storm ");
        assert_eq!(panel.sections[0].title, "Parcel ");
        assert_eq!(
            panel.sections[0].slots[0].label_override.as_deref(),
            Some("My ")
        );
    }

    #[test]
    fn reset_restores_true_no_override_default() {
        let mut config = defaults();
        config.reset_to_canonical();
        assert!(!config.is_custom());
        assert!(config.panels.is_empty());
    }
}
