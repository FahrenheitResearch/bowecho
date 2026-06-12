//! Dockable workspace: the egui_tiles tile tree that hosts the radar map
//! pane plus any DOCKED viewer panes (Sounding / WoFS / FARM / Satellite /
//! Model / 3D). Crate evaluation + integration design: docs/docking-spike.md.
//!
//! Division of labor:
//! - The tree owns LAYOUT ONLY. Heavy viewer state (workers, textures,
//!   soundings) stays on `ViewerApp`; a [`WorkspacePane`] is an ID telling
//!   [`WorkspaceBehavior::pane_ui`] which `ViewerApp` draw fn to call.
//! - Chrome (top bar, sidebar, status bar) stays in egui `Panel`s OUTSIDE
//!   the tree; deep-config dialogs stay floating `egui::Window`s.
//! - The default layout is a single Map pane: with default simplification
//!   options a lone pane renders with NO tab bar, so the app is
//!   pixel-identical to the pre-docking build until a viewer is docked.
//!
//! Borrow split (memo §3.2, the Rerun pattern): the tree is
//! `mem::replace`d off the app for the duration of `tree.ui` while the
//! behavior borrows `&mut ViewerApp`. Because the live tree is OFF the app
//! during the pass, anything inside it (pane bodies, tab buttons) must not
//! mutate `workspace.tree` directly — they push a [`DockRequest`] instead,
//! applied after the pass.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::ViewerApp;

/// Globally unique egui id for the workspace tree.
const TREE_ID: &str = "bowecho_workspace_tree";

/// Map-pane share of the root split when the first viewer docks.
const MAP_SHARE_ON_FIRST_DOCK: f32 = 0.62;

/// Sounding-pane share when it opens the dock area: a full-width bottom
/// strip ~1/3 of the window tall (field request), where its 4:3 canvas
/// reads at analyst size instead of letterboxing in a tall column.
const SOUNDING_DOCK_SHARE: f32 = 0.34;

/// Persisted-layout schema version (`AppSettings::workspace_layout`).
const LAYOUT_VERSION: u32 = 1;

/// A pane in the workspace tile tree — an ID, not state. Serializes inside
/// the tree JSON, so a saved layout intrinsically captures what was docked
/// where.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WorkspacePane {
    /// The radar map. The 1/2/4 radar grid lives INSIDE this single pane
    /// (`map_canvas`); radar cells are deliberately NOT tiles in v1 — they
    /// share one geo transform + focus logic the sidebar edits.
    Map,
    Sounding,
    Wofs,
    Farm,
    Satellite,
    Model,
    Vol3d,
}

impl WorkspacePane {
    /// Every dockable viewer (everything but the map anchor).
    pub const VIEWERS: [Self; 6] = [
        Self::Sounding,
        Self::Wofs,
        Self::Farm,
        Self::Satellite,
        Self::Model,
        Self::Vol3d,
    ];

    /// Short tab title — full names stay on the floating windows.
    pub fn tab_title(self) -> &'static str {
        match self {
            Self::Map => "Radar",
            Self::Sounding => "Sounding",
            Self::Wofs => "WoFS",
            Self::Farm => "FARM",
            Self::Satellite => "Satellite",
            Self::Model => "Model",
            Self::Vol3d => "3D Volume",
        }
    }
}

/// Tri-state visibility for a dockable viewer. `Hidden` and `Floating`
/// are exactly the pre-docking states (closed / `egui::Window`); `Docked`
/// renders the same body as a tile pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewerMode {
    #[default]
    Hidden,
    Floating,
    Docked,
}

/// Deferred dock mutation, requested from inside the tree pass (tab close
/// button / tab context menu) and applied by
/// `ViewerApp::apply_dock_requests` once the live tree is back on the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockRequest {
    /// Pane leaves the tree; the viewer reopens as a floating window
    /// (memo: tab close → Floating, so nothing is ever "lost").
    Float(WorkspacePane),
    /// Pane leaves the tree; the viewer hides. `prefer_docked` keeps the
    /// memory so reopening it returns it to the dock.
    Hide(WorkspacePane),
}

/// The workspace layout: the tile tree plus dock bookkeeping.
pub struct Workspace {
    pub tree: egui_tiles::Tree<WorkspacePane>,
    /// Viewers that should return DOCKED the next time they open (set on
    /// dock, kept across hide, cleared when the user floats the pane).
    pub prefer_docked: BTreeSet<WorkspacePane>,
    /// Requests raised during the tree pass — see [`DockRequest`].
    pub requests: Vec<DockRequest>,
    /// Layout changed (dock/undock/drag/resize/tri-state) — persist after
    /// a debounce so a split-drag doesn't write config.json every frame.
    pub dirty: bool,
    /// When the layout last changed (drives the persist debounce).
    pub last_edit: Option<Instant>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            tree: default_tree(),
            prefer_docked: BTreeSet::new(),
            requests: Vec::new(),
            dirty: false,
            last_edit: None,
        }
    }
}

/// What `AppSettings::workspace_layout` holds. Versioned for forward
/// migration; any parse failure yields the default layout (the settings
/// crate's best-effort philosophy).
#[derive(Serialize, Deserialize)]
struct PersistedLayout {
    version: u32,
    tree: egui_tiles::Tree<WorkspacePane>,
    /// Tri-state per viewer at save time (restores open windows + panes).
    viewers: BTreeMap<WorkspacePane, ViewerMode>,
    prefer_docked: BTreeSet<WorkspacePane>,
}

/// The default tree: the map pane alone, filling everything.
pub fn default_tree() -> egui_tiles::Tree<WorkspacePane> {
    let mut tiles = egui_tiles::Tiles::default();
    let map = tiles.insert_pane(WorkspacePane::Map);
    egui_tiles::Tree::new(TREE_ID, map, tiles)
}

/// A placeholder for `mem::replace` while the live tree runs its ui pass.
/// Never shown, never persisted.
pub fn placeholder_tree() -> egui_tiles::Tree<WorkspacePane> {
    egui_tiles::Tree::empty(TREE_ID)
}

impl Workspace {
    pub fn is_docked(&self, pane: WorkspacePane) -> bool {
        self.tree.tiles.find_pane(&pane).is_some()
    }

    /// Layout changed: queue a (debounced) persist.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        self.last_edit = Some(Instant::now());
    }

    /// Snapshot for `AppSettings::workspace_layout`. `viewers` is the
    /// app's tri-state per dockable viewer at save time.
    pub fn to_persisted(&self, viewers: BTreeMap<WorkspacePane, ViewerMode>) -> serde_json::Value {
        serde_json::to_value(PersistedLayout {
            version: LAYOUT_VERSION,
            tree: self.tree.clone(),
            viewers,
            prefer_docked: self.prefer_docked.clone(),
        })
        .unwrap_or(serde_json::Value::Null)
    }

    /// Best-effort restore from `AppSettings::workspace_layout`. On
    /// success the tree + dock preferences are replaced and the saved
    /// viewer tri-states are returned for the app to apply to its open
    /// flags; `None` (parse failure / future version) leaves the default.
    pub fn restore_persisted(
        &mut self,
        value: &serde_json::Value,
    ) -> Option<BTreeMap<WorkspacePane, ViewerMode>> {
        let persisted: PersistedLayout = serde_json::from_value(value.clone()).ok()?;
        if persisted.version != LAYOUT_VERSION {
            return None;
        }
        self.tree = persisted.tree;
        self.prefer_docked = persisted.prefer_docked;
        self.ensure_map_pane();
        Some(persisted.viewers)
    }

    /// Insert a viewer pane into the tree. First dock splits the root
    /// (map keeps ~62%); later docks tab next to an existing viewer pane
    /// so viewers stack as tabs by default (drag to split afterwards).
    pub fn dock(&mut self, pane: WorkspacePane) {
        if pane == WorkspacePane::Map || self.is_docked(pane) {
            return;
        }
        self.prefer_docked.insert(pane);
        let child = self.tree.tiles.insert_pane(pane);
        // Prefer tabbing alongside an already-docked viewer.
        let sibling = WorkspacePane::VIEWERS
            .iter()
            .filter(|other| **other != pane)
            .find_map(|other| self.tree.tiles.find_pane(other));
        if let Some(sibling) = sibling
            && let Some(parent) = self.tree.tiles.parent_of(sibling)
            && let Some(egui_tiles::Tile::Container(egui_tiles::Container::Tabs(tabs))) =
                self.tree.tiles.get_mut(parent)
        {
            tabs.add_child(child);
            tabs.set_active(child);
            self.mark_dirty();
            return;
        }
        // No viewer tabs to join: open the dock area as a split. The
        // Sounding's 4:3 canvas letterboxed badly in the tall right
        // column (field report: "soundings need like 1/3rd the height
        // window") — it docks as a full-width bottom strip instead;
        // other viewers keep the right-hand split. Drag the divider to
        // taste; shares persist in the saved layout.
        let (direction, map_share) = if pane == WorkspacePane::Sounding {
            (egui_tiles::LinearDir::Vertical, 1.0 - SOUNDING_DOCK_SHARE)
        } else {
            (egui_tiles::LinearDir::Horizontal, MAP_SHARE_ON_FIRST_DOCK)
        };
        let tabs_id = self.tree.tiles.insert_tab_tile(vec![child]);
        match self.tree.root {
            Some(old_root) => {
                let new_root = self.tree.tiles.insert_new(egui_tiles::Tile::Container(
                    egui_tiles::Container::Linear(egui_tiles::Linear::new_binary(
                        direction,
                        [old_root, tabs_id],
                        map_share,
                    )),
                ));
                self.tree.root = Some(new_root);
            }
            None => self.tree.root = Some(tabs_id),
        }
        self.mark_dirty();
    }

    /// Remove a viewer pane from the tree (the per-frame simplification
    /// pass prunes any container left empty or single-child).
    pub fn undock(&mut self, pane: WorkspacePane) {
        if pane == WorkspacePane::Map {
            return;
        }
        if let Some(tile_id) = self.tree.tiles.find_pane(&pane) {
            self.tree.remove_recursively(tile_id);
            self.mark_dirty();
        }
    }

    /// The map pane is the workspace anchor — a corrupt persisted layout
    /// (or a hostile config.json edit) must never leave the app mapless.
    pub fn ensure_map_pane(&mut self) {
        if self.tree.tiles.find_pane(&WorkspacePane::Map).is_none() {
            self.tree = default_tree();
            self.prefer_docked.clear();
            self.mark_dirty();
        }
    }

    /// Drop every viewer pane, returning the default map-only layout.
    pub fn reset(&mut self) {
        self.tree = default_tree();
        self.prefer_docked.clear();
        self.mark_dirty();
    }
}

/// `egui_tiles::Behavior` bridging the tree to `ViewerApp` draw fns.
pub struct WorkspaceBehavior<'a> {
    pub app: &'a mut ViewerApp,
}

impl egui_tiles::Behavior<WorkspacePane> for WorkspaceBehavior<'_> {
    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        _tile_id: egui_tiles::TileId,
        pane: &mut WorkspacePane,
    ) -> egui_tiles::UiResponse {
        match pane {
            WorkspacePane::Map => self.app.map_canvas(ui),
            viewer => self.app.docked_viewer_body(ui, *viewer),
        }
        // Never DragStarted: a body drag must stay a map pan / viewer
        // interaction. Tiles rearrange from their tabs only.
        egui_tiles::UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &WorkspacePane) -> egui::WidgetText {
        pane.tab_title().into()
    }

    fn is_tab_closable(
        &self,
        tiles: &egui_tiles::Tiles<WorkspacePane>,
        tile_id: egui_tiles::TileId,
    ) -> bool {
        // The map anchor is permanent; viewer tabs close back to floating.
        !matches!(tiles.get_pane(&tile_id), Some(WorkspacePane::Map))
    }

    fn on_tab_close(
        &mut self,
        tiles: &mut egui_tiles::Tiles<WorkspacePane>,
        tile_id: egui_tiles::TileId,
    ) -> bool {
        // egui_tiles removes the tile itself when we return true; we only
        // record the viewer-state flip (docked → floating window).
        if let Some(&pane) = tiles.get_pane(&tile_id) {
            self.app.workspace.requests.push(DockRequest::Float(pane));
        }
        true
    }

    fn on_tab_button(
        &mut self,
        tiles: &mut egui_tiles::Tiles<WorkspacePane>,
        tile_id: egui_tiles::TileId,
        button_response: egui::Response,
    ) -> egui::Response {
        let Some(&pane) = tiles.get_pane(&tile_id) else {
            return button_response;
        };
        if pane == WorkspacePane::Map {
            return button_response;
        }
        button_response.context_menu(|ui| {
            if ui.button("Float as window").clicked() {
                self.app.workspace.requests.push(DockRequest::Float(pane));
                ui.close();
            }
            if ui.button("Hide").clicked() {
                self.app.workspace.requests.push(DockRequest::Hide(pane));
                ui.close();
            }
        });
        button_response
            .on_hover_text("Drag to rearrange · right-click to float/hide · ✕ floats as a window")
    }

    fn on_edit(&mut self, _edit_action: egui_tiles::EditAction) {
        // Tab switches, drops and share resizes all change the serialized
        // tree — mark for the debounced persist.
        self.app.workspace.mark_dirty();
    }

    fn simplification_options(&self) -> egui_tiles::SimplificationOptions {
        egui_tiles::SimplificationOptions {
            // Every pane keeps its tab: the tab IS the drag handle for
            // grid rearrangement (field request: "why cant we drag n
            // drop"). The default pruned lone Tabs containers, so a
            // single docked pane had nothing to grab — drag a tab onto
            // any pane edge to split, onto a tab strip to stack.
            all_panes_must_have_tabs: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tree_is_a_lone_map_pane() {
        let workspace = Workspace::default();
        assert_eq!(workspace.tree.tiles.len(), 1);
        assert!(workspace.is_docked(WorkspacePane::Map));
        for viewer in WorkspacePane::VIEWERS {
            assert!(!workspace.is_docked(viewer));
        }
    }

    #[test]
    fn dock_undock_round_trip_restores_map_only_tree() {
        let mut workspace = Workspace::default();
        workspace.dock(WorkspacePane::Wofs);
        assert!(workspace.is_docked(WorkspacePane::Wofs));
        assert!(workspace.prefer_docked.contains(&WorkspacePane::Wofs));
        // Second viewer tabs next to the first (one Tabs container).
        workspace.dock(WorkspacePane::Sounding);
        assert!(workspace.is_docked(WorkspacePane::Sounding));
        let wofs = workspace
            .tree
            .tiles
            .find_pane(&WorkspacePane::Wofs)
            .unwrap();
        let sounding = workspace
            .tree
            .tiles
            .find_pane(&WorkspacePane::Sounding)
            .unwrap();
        assert_eq!(
            workspace.tree.tiles.parent_of(wofs),
            workspace.tree.tiles.parent_of(sounding),
            "viewers stack as tabs by default"
        );
        workspace.undock(WorkspacePane::Wofs);
        workspace.undock(WorkspacePane::Sounding);
        assert!(!workspace.is_docked(WorkspacePane::Wofs));
        assert!(!workspace.is_docked(WorkspacePane::Sounding));
        // Simplification (run by tree.ui each frame) prunes the leftover
        // containers; the map pane itself must still be present.
        assert!(workspace.is_docked(WorkspacePane::Map));
    }

    #[test]
    fn map_pane_cannot_be_docked_twice_or_undocked() {
        let mut workspace = Workspace::default();
        workspace.dock(WorkspacePane::Map);
        assert_eq!(workspace.tree.tiles.len(), 1);
        workspace.undock(WorkspacePane::Map);
        assert!(workspace.is_docked(WorkspacePane::Map));
    }

    #[test]
    fn ensure_map_pane_recovers_from_a_mapless_tree() {
        let mut workspace = Workspace {
            tree: egui_tiles::Tree::empty(TREE_ID),
            ..Default::default()
        };
        assert!(!workspace.is_docked(WorkspacePane::Map));
        workspace.ensure_map_pane();
        assert!(workspace.is_docked(WorkspacePane::Map));
    }

    #[test]
    fn tree_json_round_trips_with_docked_viewers() {
        let mut workspace = Workspace::default();
        workspace.dock(WorkspacePane::Vol3d);
        workspace.dock(WorkspacePane::Satellite);
        let json = serde_json::to_string(&workspace.tree).expect("serialize");
        let back: egui_tiles::Tree<WorkspacePane> =
            serde_json::from_str(&json).expect("deserialize");
        assert!(back == workspace.tree);
    }

    #[test]
    fn persisted_layout_round_trips_tree_modes_and_preferences() {
        let mut workspace = Workspace::default();
        workspace.dock(WorkspacePane::Wofs);
        workspace.prefer_docked.insert(WorkspacePane::Vol3d); // hidden, was docked
        let viewers: BTreeMap<WorkspacePane, ViewerMode> = [
            (WorkspacePane::Wofs, ViewerMode::Docked),
            (WorkspacePane::Satellite, ViewerMode::Floating),
            (WorkspacePane::Vol3d, ViewerMode::Hidden),
        ]
        .into_iter()
        .collect();
        let value = workspace.to_persisted(viewers.clone());

        let mut restored = Workspace::default();
        let back = restored.restore_persisted(&value).expect("layout restores");
        assert_eq!(back, viewers);
        assert!(restored.is_docked(WorkspacePane::Map));
        assert!(restored.is_docked(WorkspacePane::Wofs));
        assert!(restored.prefer_docked.contains(&WorkspacePane::Wofs));
        assert!(restored.prefer_docked.contains(&WorkspacePane::Vol3d));
        assert!(restored.tree == workspace.tree);
    }

    #[test]
    fn persisted_layout_rejects_garbage_and_future_versions() {
        let mut workspace = Workspace::default();
        assert!(
            workspace
                .restore_persisted(&serde_json::json!({"bogus": true}))
                .is_none()
        );
        let mut future = workspace.to_persisted(BTreeMap::new());
        future["version"] = serde_json::json!(999);
        assert!(workspace.restore_persisted(&future).is_none());
        // Failures leave the default layout intact.
        assert!(workspace.is_docked(WorkspacePane::Map));
        assert_eq!(workspace.tree.tiles.len(), 1);
    }

    /// Manual-QA helper, not an assertion: writes a config.json whose
    /// layout has the 3D Volume pane DOCKED, for booting the app against
    /// a sandbox APPDATA (verifies restore + the wgpu paint callback
    /// inside a tile pane without clicking through the UI).
    ///
    /// `cargo test -p app_ui dump_docked_vol3d -- --ignored --nocapture`
    /// then run bowecho with APPDATA pointing at a dir containing
    /// `bowecho/config.json` = the dumped file.
    #[test]
    #[ignore = "manual QA helper — writes %TEMP%/bowecho_qa_config.json"]
    fn dump_docked_vol3d_config_for_manual_qa() {
        let mut workspace = Workspace::default();
        workspace.dock(WorkspacePane::Vol3d);
        let viewers: BTreeMap<WorkspacePane, ViewerMode> =
            [(WorkspacePane::Vol3d, ViewerMode::Docked)]
                .into_iter()
                .collect();
        let settings = settings::AppSettings {
            workspace_layout: Some(workspace.to_persisted(viewers)),
            ..Default::default()
        };
        let path = std::env::temp_dir().join("bowecho_qa_config.json");
        std::fs::write(&path, settings.to_json()).expect("write QA config");
        println!("wrote {}", path.display());
    }

    #[test]
    fn restore_recovers_a_mapless_persisted_tree() {
        let mut mapless = Workspace {
            tree: egui_tiles::Tree::empty(TREE_ID),
            ..Default::default()
        };
        mapless.prefer_docked.insert(WorkspacePane::Farm);
        let value = mapless.to_persisted(BTreeMap::new());
        let mut restored = Workspace::default();
        assert!(restored.restore_persisted(&value).is_some());
        // ensure_map_pane ran: the map is back, the bad layout is gone.
        assert!(restored.is_docked(WorkspacePane::Map));
    }
}
