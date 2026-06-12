//! Docking spike (docs/docking-spike.md) recreated on this tree: validates
//! egui_tiles 0.15 against our exact egui/eframe before the real
//! integration in `src/dock.rs`.
//!
//! Run: `cargo run -p app_ui --example dock_spike`
//!
//! What it proves live:
//! 1. A custom-painter "map" pane gets a correct rect + clip, and a body
//!    drag pans the map (moves the crosshair) — it never relocates the tile
//!    (`pane_ui` returns `UiResponse::None`).
//! 2. "Sounding" panes intentionally overdraw past their right edge and get
//!    clipped at the tile boundary.
//! 3. Tabs + splits + drag-to-rearrange + resize work with zero code beyond
//!    `tree.ui(...)`.
//! 4. `Tree<SpikePane>` round-trips through serde_json (printed at startup,
//!    Save/Restore buttons in the corner window).
//! 5. A floating `egui::Window` renders above the tile tree.

use eframe::egui;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
enum SpikePane {
    Map,
    Sounding { name: String, hue: f32 },
}

struct SpikeBehavior<'a> {
    map_pan: &'a mut egui::Vec2,
}

impl egui_tiles::Behavior<SpikePane> for SpikeBehavior<'_> {
    fn tab_title_for_pane(&mut self, pane: &SpikePane) -> egui::WidgetText {
        match pane {
            SpikePane::Map => "Map".into(),
            SpikePane::Sounding { name, .. } => name.clone().into(),
        }
    }

    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        _tile_id: egui_tiles::TileId,
        pane: &mut SpikePane,
    ) -> egui_tiles::UiResponse {
        let rect = ui.max_rect();
        let painter = ui.painter();
        match pane {
            SpikePane::Map => {
                // Full-rect gradient stand-in for the radar map.
                let mut mesh = egui::Mesh::default();
                let c0 = egui::Color32::from_rgb(10, 16, 28);
                let c1 = egui::Color32::from_rgb(30, 60, 90);
                mesh.colored_vertex(rect.left_top(), c0);
                mesh.colored_vertex(rect.right_top(), c1);
                mesh.colored_vertex(rect.right_bottom(), c0);
                mesh.colored_vertex(rect.left_bottom(), c1);
                mesh.add_triangle(0, 1, 2);
                mesh.add_triangle(0, 2, 3);
                painter.add(mesh);
                // Body drag = map pan (crosshair moves), NEVER a tile drag:
                // we return UiResponse::None below, so the tree cannot
                // relocate this tile from a body drag (tree.rs:406).
                let response = ui.interact(
                    rect,
                    ui.id().with("map_drag"),
                    egui::Sense::click_and_drag(),
                );
                if response.dragged() {
                    *self.map_pan += response.drag_delta();
                }
                let cross = rect.center() + *self.map_pan;
                let stroke = egui::Stroke::new(1.5, egui::Color32::from_rgb(120, 200, 120));
                painter.line_segment(
                    [
                        egui::pos2(cross.x - 14.0, cross.y),
                        egui::pos2(cross.x + 14.0, cross.y),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        egui::pos2(cross.x, cross.y - 14.0),
                        egui::pos2(cross.x, cross.y + 14.0),
                    ],
                    stroke,
                );
                painter.text(
                    rect.left_top() + egui::vec2(8.0, 8.0),
                    egui::Align2::LEFT_TOP,
                    format!(
                        "map pane — drag pans (never relocates)\nmax_rect {:?}\nclip {:?}",
                        rect,
                        ui.clip_rect()
                    ),
                    egui::FontId::monospace(11.0),
                    egui::Color32::from_rgb(200, 210, 220),
                );
            }
            SpikePane::Sounding { name, hue } => {
                let color: egui::Color32 = egui::ecolor::Hsva::new(*hue, 0.5, 0.35, 1.0).into();
                painter.rect_filled(rect, 0.0, color);
                // Deliberate overdraw PAST the tile's right edge: the pane
                // Ui's clip rect must cut it at the boundary.
                painter.line_segment(
                    [
                        egui::pos2(rect.left() + 10.0, rect.bottom() - 10.0),
                        egui::pos2(rect.right() + 400.0, rect.top() - 40.0),
                    ],
                    egui::Stroke::new(3.0, egui::Color32::YELLOW),
                );
                painter.text(
                    rect.left_top() + egui::vec2(8.0, 8.0),
                    egui::Align2::LEFT_TOP,
                    format!("{name}: yellow trace is clipped at the tile edge"),
                    egui::FontId::monospace(11.0),
                    egui::Color32::WHITE,
                );
            }
        }
        egui_tiles::UiResponse::None
    }

    fn is_tab_closable(
        &self,
        tiles: &egui_tiles::Tiles<SpikePane>,
        tile_id: egui_tiles::TileId,
    ) -> bool {
        // The map anchor is permanent; viewers close.
        !matches!(tiles.get_pane(&tile_id), Some(SpikePane::Map))
    }
}

struct SpikeApp {
    tree: egui_tiles::Tree<SpikePane>,
    map_pan: egui::Vec2,
    saved: Option<String>,
    show_floating: bool,
}

fn default_tree() -> egui_tiles::Tree<SpikePane> {
    let mut tiles = egui_tiles::Tiles::default();
    let map = tiles.insert_pane(SpikePane::Map);
    let s1 = tiles.insert_pane(SpikePane::Sounding {
        name: "Sounding A".to_owned(),
        hue: 0.05,
    });
    let s2 = tiles.insert_pane(SpikePane::Sounding {
        name: "Sounding B".to_owned(),
        hue: 0.6,
    });
    let tabs = tiles.insert_tab_tile(vec![s1, s2]);
    let root = tiles.insert_new(egui_tiles::Tile::Container(egui_tiles::Container::Linear(
        egui_tiles::Linear::new_binary(egui_tiles::LinearDir::Horizontal, [map, tabs], 0.65),
    )));
    egui_tiles::Tree::new("dock_spike_tree", root, tiles)
}

impl Default for SpikeApp {
    fn default() -> Self {
        let tree = default_tree();
        // Startup proof: the tree JSON round-trips with equality.
        let json = serde_json::to_string(&tree).expect("tree serializes");
        let back: egui_tiles::Tree<SpikePane> =
            serde_json::from_str(&json).expect("tree deserializes");
        assert!(back == tree, "tree JSON round-trip must be lossless");
        println!("startup JSON round-trip OK ({} bytes)", json.len());
        Self {
            tree,
            map_pan: egui::Vec2::ZERO,
            saved: None,
            show_floating: true,
        }
    }
}

impl eframe::App for SpikeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().show_inside(ui, |ui| {
            let mut behavior = SpikeBehavior {
                map_pan: &mut self.map_pan,
            };
            self.tree.ui(&mut behavior, ui);
        });
        // A floating window above the tree — deep-config dialogs keep this.
        if self.show_floating {
            let mut open = self.show_floating;
            egui::Window::new("Floating above the tree")
                .open(&mut open)
                .default_size([320.0, 180.0])
                .show(&ctx, |ui| {
                    ui.label("egui::Window z-order over tile panes — verified.");
                    ui.horizontal(|ui| {
                        if ui.button("Save layout").clicked() {
                            self.saved = serde_json::to_string(&self.tree).ok();
                        }
                        let restore = ui
                            .add_enabled(self.saved.is_some(), egui::Button::new("Restore layout"));
                        if restore.clicked()
                            && let Some(json) = &self.saved
                            && let Ok(tree) = serde_json::from_str(json)
                        {
                            self.tree = tree;
                        }
                        if ui.button("Reset").clicked() {
                            self.tree = default_tree();
                        }
                    });
                    if let Some(json) = &self.saved {
                        ui.weak(format!("saved snapshot: {} bytes", json.len()));
                    }
                });
            self.show_floating = open;
        }
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "BowEcho dock spike",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(SpikeApp::default()))),
    )
}
