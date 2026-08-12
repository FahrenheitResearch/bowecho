// SPDX-License-Identifier: Apache-2.0

//! The map canvas: one persistent camera through design → run → analyze.
//! Pan/zoom, place search overlay, domain draw/resize/move, basemap
//! painting, and the hover readout. Domain geometry edits happen in the
//! domain's own grid space (exact Lambert inverse), never in screen space.

use eframe::egui;

use arwen_map::LambertDomain;
use arwen_map::basemap_data;
use arwen_map::search::{self, CoordinateMarker, PlaceSearchResult};
use arwen_map::tiles::{TileLayer, TileSource, TileStyle};
use arwen_map::view::MapView;

use crate::draft::Draft;
use crate::run_session::RunSession;
use crate::theme::theme;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Handle {
    /// 0 SW, 1 SE, 2 NE, 3 NW (grid-corner order).
    Corner(u8),
    /// 0 S, 1 E, 2 N, 3 W.
    Edge(u8),
    Interior,
}

#[derive(Clone, Debug)]
enum DragOp {
    Pan,
    Draw {
        anchor_lon_lat: (f64, f64),
    },
    MoveDomain {
        grab_offset_deg: (f64, f64),
    },
    Resize {
        handle: Handle,
    },
    /// Drag one fitted NEST inside its parent; the grab offset is in the
    /// parent's grid cells so the drop snaps to whole-cell anchors.
    MoveNest {
        grid_id: u32,
        grab_offset: (f64, f64),
    },
    /// Resize one fitted nest by a handle, snapped to whole parent cells.
    ResizeNest {
        grid_id: u32,
        handle: Handle,
    },
    /// Drag the spawn-trigger search box inside the spawning nest's
    /// parent (armed from the spawn card).
    SearchBox {
        domain_index: usize,
        grid_id: u32,
        anchor: (f64, f64),
    },
}

/// A placement change produced by a map gesture, in the WHOLE-CELL
/// integers the config's `[[domain]]` keys take. The app writes it into
/// the working config (exactly as the cards write their blocks) and the
/// debounced engine `--resolve` remains the judge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlacementEdit {
    Place {
        grid_id: u32,
        i_parent_start: i64,
        j_parent_start: i64,
        /// `Some` on resize (new nx/ny), `None` on a pure move.
        size: Option<(u32, u32)>,
    },
    SpawnSearchBox {
        domain_index: usize,
        /// 1-based inclusive parent cells `[i_lo, j_lo, i_hi, j_hi]`.
        bounds: [i64; 4],
    },
    /// A COMPLETED root draw (or redraw): THE DRAWN RECTANGLE IS THE
    /// DOMAIN. The app writes the drawn whole-cell size as a MANUAL
    /// root nx/ny on the config surface — regenerating the surface for
    /// the new rectangle first (children reset to the engine's fit).
    /// The engine's VRAM fit becomes the estimate strip's verdict on
    /// this shape, never a silent override of it.
    RootDrawn { nx: u32, ny: u32 },
    /// A root handle-resize or whole-root move finished: the same rect
    /// authority, written INTO the existing surface in place (center +
    /// size), knob edits kept. Without a surface it rides the same
    /// auto-generate as [`PlacementEdit::RootDrawn`].
    RootAdjusted { nx: u32, ny: u32 },
}

/// The dragged nest painted live: whole-cell placement in its parent.
#[derive(Clone, Copy, Debug, PartialEq)]
struct NestPreview {
    grid_id: u32,
    i: i64,
    j: i64,
    nx: u32,
    ny: u32,
    resized: bool,
}

#[derive(Default)]
pub struct MapPane {
    pub draw_mode: bool,
    drag: Option<DragOp>,
    /// Live preview rect while drawing (geo corners).
    preview: Option<((f64, f64), (f64, f64))>,
    /// The nest whose resize handles are shown (click or drag selects).
    pub selected_nest: Option<u32>,
    /// The nest under the cursor (hover highlight + move cursor).
    pub hovered_nest: Option<u32>,
    /// Re-arming "Draw domain" over an existing domain asks first —
    /// completing the new drag REPLACES the parent (children placements
    /// reset with it).
    pub confirm_redraw: bool,
    /// Live nest placement while (and after) a nest drag, until the
    /// resolved tree catches up — the outline never snaps back to the
    /// pre-drag anchor while the engine is re-resolving.
    nest_preview: Option<NestPreview>,
    /// Spawn search-box drawing mode: `(domain_index, grid_id)` of the
    /// spawning nest, armed from the spawn card.
    pub search_box_arm: Option<(usize, u32)>,
    /// Live search-box preview: bounds + the spawning nest's grid id.
    search_preview: Option<([i64; 4], u32)>,
    /// Whole-cell placement edits for the app to write into the config.
    pub placement_edits: Vec<PlacementEdit>,
    pub search_query: String,
    pub search_results: Vec<PlaceSearchResult>,
    pub search_error: Option<String>,
    pub marker: Option<CoordinateMarker>,
    /// Where the search field was laid out last frame (synthesized-input
    /// tests aim their pointer here).
    pub last_search_rect: Option<egui::Rect>,
    /// The canvas rect of the last frame (synthesized-input tests project
    /// gesture targets through the app's real camera with it).
    pub last_canvas_rect: Option<egui::Rect>,
}

/// Everything the canvas needs for one frame.
pub struct MapFrame<'a> {
    pub view: &'a mut MapView,
    pub tiles: &'a mut TileLayer,
    pub tile_style: TileStyle,
    pub draft: &'a mut Draft,
    pub session: Option<&'a RunSession>,
    /// Draft editing allowed (New view, no run in flight on this draft).
    pub editable: bool,
    /// The loaded model field layer (composite reflectivity).
    pub model: Option<&'a crate::model_layer::ModelLayer>,
    /// The ENGINE-fitted domain from an open review — the answer to the
    /// sketch, painted stronger than the sketch.
    pub fitted: Option<&'a LambertDomain>,
    /// The whole ENGINE-fitted tree (root + nests, parent-before-child,
    /// placements verbatim); empty when no review is open.
    pub fitted_tree: &'a [arwen_plan::queries::ResolvedDomain],
    /// The tree is a draft-derived PLACEHOLDER sketch (the bridge until
    /// the first engine fit answers): painted dashed with a sketch tag,
    /// still fully selectable/draggable.
    pub tree_provisional: bool,
    /// The engine's reported boundary clearance (`domain_size_floor.
    /// clearance_rows`): the keepout band inside every parent's edge.
    /// `None` = not reported; no band is painted and drags clamp only
    /// to the parent itself.
    pub clearance_rows: Option<f64>,
    /// `domain_size_floor.nest_span_mass_points`: the engine's smallest
    /// legal nest span, the resize floor. `None` = not reported.
    pub min_span_points: Option<u32>,
    /// The engine's refusal sentence for the current working config, if
    /// its last `--resolve` refused — painted at the canvas foot so a
    /// stopped drag explains itself.
    pub placement_refusal: Option<String>,
    /// Domains the refusal NAMES (`grid_id = N` in the engine's own
    /// sentence): each paints with a red outline and its sentence
    /// anchored to it, so a refused placement points at itself.
    pub refusal_domains: Vec<(u32, String)>,
    /// Declared spawn search boxes from the working config:
    /// `(spawning grid_id, [i_lo, j_lo, i_hi, j_hi])` in the spawning
    /// nest's PARENT cells.
    pub search_boxes: Vec<(u32, [i64; 4])>,
}

/// One nest's gesture geometry, derived from the fitted tree: the
/// placement chain from the root (with the grid id of every level, so a
/// dragged ancestor carries its children in the preview) and the PARENT
/// grid its `i_parent_start` lives in.
struct NestGeom {
    grid_id: u32,
    /// Root→nest placements; the last element IS this nest.
    path: Vec<arwen_map::NestPlacement>,
    /// grid_id of each `path` level, aligned.
    ids: Vec<u32>,
    parent_nx: u32,
    parent_ny: u32,
}

impl NestGeom {
    fn placement(&self) -> &arwen_map::NestPlacement {
        self.path.last().expect("path ends at this nest")
    }

    fn ratio(&self) -> f64 {
        self.placement().parent_grid_ratio.max(1.0)
    }

    /// Does this nest parent another nest of the tree?
    fn has_children(&self, tree: &[arwen_plan::queries::ResolvedDomain]) -> bool {
        tree.iter().any(|domain| domain.parent_id == self.grid_id)
    }
}

/// Gesture geometry for every nest of the fitted tree, parents before
/// children (paint order) — reverse for deepest-first hit testing.
fn nest_geoms(frame: &MapFrame<'_>) -> Vec<NestGeom> {
    frame
        .fitted_tree
        .iter()
        .filter(|domain| domain.parent_id != 0)
        .filter_map(|nest| {
            let mut path = Vec::new();
            let mut ids = Vec::new();
            let mut current = nest.clone();
            loop {
                path.push(arwen_map::NestPlacement {
                    i_parent_start: current.i_parent_start,
                    j_parent_start: current.j_parent_start,
                    parent_grid_ratio: current.parent_grid_ratio,
                    nx: current.nx,
                    ny: current.ny,
                });
                ids.push(current.grid_id);
                let parent = frame
                    .fitted_tree
                    .iter()
                    .find(|domain| domain.grid_id == current.parent_id)?;
                if parent.parent_id == 0 {
                    break;
                }
                current = parent.clone();
            }
            path.reverse();
            ids.reverse();
            let parent = frame
                .fitted_tree
                .iter()
                .find(|domain| domain.grid_id == nest.parent_id)?;
            Some(NestGeom {
                grid_id: nest.grid_id,
                path,
                ids,
                parent_nx: parent.nx,
                parent_ny: parent.ny,
            })
        })
        .collect()
}

impl MapPane {
    /// Returns the pane rect so the app can paint run layers between
    /// basemap and chrome next frame.
    pub fn ui(&mut self, ui: &mut egui::Ui, frame: &mut MapFrame<'_>) -> egui::Rect {
        let rect = ui.available_rect_before_wrap();
        self.last_canvas_rect = Some(rect);
        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        painter.rect_filled(rect, 0.0, theme().map_bg);
        self.paint_basemap(&painter, rect, frame);
        if let Some(model) = frame.model {
            model.paint(&painter, rect, frame.view);
        }
        self.handle_input(ui, rect, &response, frame);
        self.paint_domain(&painter, rect, frame);
        self.paint_marker(&painter, rect, frame);
        self.paint_hover_readout(ui, rect, &response, frame);
        self.paint_model_error(&painter, rect, frame);
        self.paint_placement_refusal(&painter, rect, frame);
        self.paint_mode_chip(&painter, rect, frame);
        self.paint_selection_hint(&painter, rect, frame);
        self.paint_attribution(&painter, rect, frame);
        rect
    }

    /// The mode chip (top-left): which tool the next press uses is never
    /// a guess — DRAWING is a mode you can see and leave (Esc).
    fn paint_mode_chip(&self, painter: &egui::Painter, rect: egui::Rect, frame: &MapFrame<'_>) {
        if !frame.editable {
            return;
        }
        let (text, color) = if self.draw_mode {
            (
                "DRAWING — drag a rectangle for the parent · Esc exits",
                theme().warn,
            )
        } else if self.search_box_arm.is_some() {
            (
                "SEARCH BOX — drag inside the parent · Esc cancels",
                theme().warn,
            )
        } else if frame.draft.domain.is_some() {
            (
                "editing — click selects a domain · drag moves it",
                theme().text_weak,
            )
        } else {
            return; // the empty-state invitation already explains
        };
        let galley = painter.layout_no_wrap(text.into(), egui::FontId::proportional(10.5), color);
        let padding = egui::vec2(7.0, 4.0);
        let card = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 8.0, rect.top() + 8.0),
            galley.size() + padding * 2.0,
        );
        painter.rect_filled(
            card,
            3.0,
            egui::Color32::from_rgba_unmultiplied(15, 14, 13, 200),
        );
        painter.galley(card.min + padding, galley, color);
    }

    /// One line under the map while a nest is selected: what the
    /// gestures do and where the size fields live.
    fn paint_selection_hint(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        frame: &MapFrame<'_>,
    ) {
        if !frame.editable {
            return;
        }
        let Some(grid_id) = self.selected_nest else {
            return;
        };
        if !frame.fitted_tree.iter().any(|nest| nest.grid_id == grid_id) {
            return;
        }
        painter.text(
            egui::pos2(rect.center().x, rect.bottom() - 24.0),
            egui::Align2::CENTER_BOTTOM,
            format!(
                "d{grid_id:02} selected — drag to move · corner/edge handles to \
                 resize · click again for its parent · sizes editable in the \
                 inspector"
            ),
            egui::FontId::proportional(11.0),
            theme().accent_text,
        );
    }

    /// A wanted frame that cannot be shown is said out loud, never a
    /// silently empty map.
    fn paint_model_error(&self, painter: &egui::Painter, rect: egui::Rect, frame: &MapFrame<'_>) {
        let Some((_, error)) = frame.model.and_then(|model| model.error.as_ref()) else {
            return;
        };
        painter.text(
            egui::pos2(rect.center().x, rect.bottom() - 26.0),
            egui::Align2::CENTER_BOTTOM,
            error,
            egui::FontId::proportional(11.0),
            theme().warn,
        );
    }

    fn paint_basemap(&self, painter: &egui::Painter, rect: egui::Rect, frame: &mut MapFrame<'_>) {
        if frame.tile_style != TileStyle::DarkVector {
            frame.view.paint_raster_tiles(
                painter,
                rect,
                frame.tiles,
                &TileSource::Basemap(frame.tile_style),
                // Subdued: the forecast data owns the saturation.
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 170),
            );
        }
        let hairline = egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(200, 205, 210, 64),
        );
        frame.view.paint_basemap_lines(
            painter,
            rect,
            basemap_data::BASEMAP_WORLD_COUNTRY_LINES,
            hairline,
        );
        let state_stroke = egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(200, 205, 210, 96),
        );
        frame.view.paint_basemap_lines(
            painter,
            rect,
            basemap_data::BASEMAP_US_STATE_LINES,
            state_stroke,
        );
    }

    fn handle_input(
        &mut self,
        ui: &egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        frame: &mut MapFrame<'_>,
    ) {
        // Scroll zoom about the cursor.
        if let Some(cursor) = response.hover_pos() {
            let scroll = ui.input(|input| input.smooth_scroll_delta.y);
            if scroll.abs() > 0.2 {
                let factor = (scroll * 0.005).exp();
                frame.view.zoom_about(rect, cursor, factor);
            }
        }

        // Once the engine's tree catches up with a dropped placement,
        // the preview retires (never a snap-back while it re-resolves).
        self.sync_nest_preview(frame);

        // Esc leaves every armed mode: search-box draw, draw mode, the
        // redraw confirm.
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.search_box_arm = None;
            self.search_preview = None;
            self.draw_mode = false;
            self.confirm_redraw = false;
        }

        // Hover affordances (no drag in progress): highlight the domain
        // under the cursor and say what a press would do via the cursor
        // icon — crosshair to draw, move over a nest, resize on handles.
        if self.drag.is_none() {
            match response.hover_pos() {
                Some(cursor) if frame.editable => {
                    self.hovered_nest = self.nest_at_cursor(rect, cursor, frame);
                    if self.draw_mode {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
                    } else if let Some(icon) = self.hover_cursor(rect, cursor, frame) {
                        ui.ctx().set_cursor_icon(icon);
                    }
                }
                Some(_) => self.hovered_nest = None,
                // Pointer elsewhere (or a headless smoke): keep the last
                // hover state rather than flickering it off.
                None => {}
            }
        }

        if response.drag_started() {
            let Some(cursor) = response.interact_pointer_pos() else {
                return;
            };
            // egui recognizes a drag only after the click-threshold
            // movement, so `cursor` is already PAST the press point.
            // Every grab decision (which nest, which handle, the anchor)
            // belongs to where the button went DOWN.
            let cursor = ui
                .input(|input| input.pointer.press_origin())
                .unwrap_or(cursor);
            let (lon, lat) = frame.view.screen_to_lon_lat(rect, cursor);
            self.drag = Some(
                if frame.editable
                    && let Some((domain_index, grid_id)) = self.search_box_arm
                    && let Some(anchor) = self.cursor_parent_cells(rect, cursor, frame, grid_id)
                {
                    DragOp::SearchBox {
                        domain_index,
                        grid_id,
                        anchor,
                    }
                } else if self.draw_mode && frame.editable {
                    DragOp::Draw {
                        anchor_lon_lat: (lon as f64, lat as f64),
                    }
                } else if frame.editable
                    && let Some(op) = self.nest_drag_op(rect, cursor, frame)
                {
                    op
                } else if frame.editable
                    && let Some(handle) = self.hit_test_handle(rect, cursor, frame)
                {
                    match handle {
                        Handle::Interior => {
                            let domain = frame.draft.domain.as_ref().expect("hit tested");
                            DragOp::MoveDomain {
                                grab_offset_deg: (
                                    lon as f64 - domain.ref_lon,
                                    lat as f64 - domain.ref_lat,
                                ),
                            }
                        }
                        other => DragOp::Resize { handle: other },
                    }
                } else {
                    DragOp::Pan
                },
            );
        } else if response.clicked() && frame.editable && !self.draw_mode {
            // A plain click selects the SMALLEST domain under the cursor;
            // clicking again cycles outward through the overlap stack, so
            // an inner nest never makes its parent unreachable.
            if let Some(cursor) = response.interact_pointer_pos() {
                let candidates = self.nest_candidates_at_cursor(rect, cursor, frame);
                self.selected_nest = match self.selected_nest {
                    Some(current) => match candidates.iter().position(|grid| *grid == current) {
                        Some(index) => candidates.get((index + 1) % candidates.len()).copied(),
                        None => candidates.first().copied(),
                    },
                    None => candidates.first().copied(),
                };
            }
        }

        if response.dragged() {
            let Some(cursor) = response.interact_pointer_pos() else {
                return;
            };
            match self.drag.clone() {
                Some(DragOp::Pan) | None => {
                    let delta = response.drag_delta();
                    if delta != egui::Vec2::ZERO {
                        frame.view.pan_by_pixels(-delta);
                    }
                }
                Some(DragOp::Draw { anchor_lon_lat }) => {
                    let (lon, lat) = frame.view.screen_to_lon_lat(rect, cursor);
                    self.preview = Some((anchor_lon_lat, (lon as f64, lat as f64)));
                }
                Some(DragOp::MoveDomain { grab_offset_deg }) => {
                    let (lon, lat) = frame.view.screen_to_lon_lat(rect, cursor);
                    if let Some(domain) = &mut frame.draft.domain {
                        domain.ref_lon =
                            arwen_map::geo::normalize_lon((lon as f64 - grab_offset_deg.0) as f32)
                                as f64;
                        domain.ref_lat = (lat as f64 - grab_offset_deg.1).clamp(-85.0, 85.0);
                        domain.stand_lon = domain.ref_lon;
                    }
                }
                Some(DragOp::Resize { handle }) => {
                    let (lon, lat) = frame.view.screen_to_lon_lat(rect, cursor);
                    if let Some(domain) = &mut frame.draft.domain {
                        resize_domain(domain, handle, lat as f64, lon as f64);
                    }
                }
                Some(DragOp::MoveNest {
                    grid_id,
                    grab_offset,
                }) => self.drag_move_nest(rect, cursor, frame, grid_id, grab_offset),
                Some(DragOp::ResizeNest { grid_id, handle }) => {
                    self.drag_resize_nest(rect, cursor, frame, grid_id, handle)
                }
                Some(DragOp::SearchBox {
                    grid_id, anchor, ..
                }) => {
                    if let Some((px, py)) = self.cursor_parent_cells(rect, cursor, frame, grid_id)
                        && let Some(geom) = nest_geoms(frame)
                            .into_iter()
                            .find(|geom| geom.grid_id == grid_id)
                    {
                        let clamp_i = |v: f64| (v.round() as i64).clamp(1, geom.parent_nx as i64);
                        let clamp_j = |v: f64| (v.round() as i64).clamp(1, geom.parent_ny as i64);
                        self.search_preview = Some((
                            [
                                clamp_i(anchor.0.min(px)),
                                clamp_j(anchor.1.min(py)),
                                clamp_i(anchor.0.max(px)),
                                clamp_j(anchor.1.max(py)),
                            ],
                            grid_id,
                        ));
                    }
                }
            }
        }

        if response.drag_stopped() {
            match self.drag.take() {
                Some(DragOp::Draw { .. }) => {
                    if let Some((a, b)) = self.preview.take()
                        && let Some(domain) = domain_from_drag(a, b, frame.draft.dx_m())
                    {
                        let (nx, ny) = (domain.nx, domain.ny);
                        frame.draft.domain = Some(domain);
                        self.draw_mode = false;
                        // The drawn rectangle IS the domain: its size
                        // rides to the config surface as a manual root
                        // write. A redraw orphans the old tree's
                        // selection and preview.
                        self.selected_nest = None;
                        self.hovered_nest = None;
                        self.nest_preview = None;
                        self.placement_edits
                            .push(PlacementEdit::RootDrawn { nx, ny });
                    }
                }
                Some(DragOp::MoveDomain { .. }) | Some(DragOp::Resize { .. }) => {
                    // Root geometry follows the gesture: the moved/
                    // resized sketch is written to the config surface
                    // (center + size) exactly like a nest drop.
                    if frame.editable
                        && let Some(domain) = frame.draft.domain.as_ref()
                    {
                        self.placement_edits.push(PlacementEdit::RootAdjusted {
                            nx: domain.nx,
                            ny: domain.ny,
                        });
                    }
                }
                Some(DragOp::MoveNest { .. }) | Some(DragOp::ResizeNest { .. }) => {
                    if let Some(preview) = self.nest_preview {
                        // Unchanged placements retire silently instead of
                        // writing a no-op into the config.
                        let unchanged = frame
                            .fitted_tree
                            .iter()
                            .find(|nest| nest.grid_id == preview.grid_id)
                            .map(|nest| {
                                nest.i_parent_start.round() as i64 == preview.i
                                    && nest.j_parent_start.round() as i64 == preview.j
                                    && (!preview.resized
                                        || (nest.nx == preview.nx && nest.ny == preview.ny))
                            })
                            .unwrap_or(true);
                        if unchanged {
                            self.nest_preview = None;
                        } else {
                            self.placement_edits.push(PlacementEdit::Place {
                                grid_id: preview.grid_id,
                                i_parent_start: preview.i,
                                j_parent_start: preview.j,
                                size: preview.resized.then_some((preview.nx, preview.ny)),
                            });
                        }
                    }
                }
                Some(DragOp::SearchBox { domain_index, .. }) => {
                    if let Some((bounds, _)) = self.search_preview.take() {
                        self.placement_edits.push(PlacementEdit::SpawnSearchBox {
                            domain_index,
                            bounds,
                        });
                    }
                    self.search_box_arm = None;
                }
                _ => {}
            }
            self.preview = None;
        }
    }

    /// Retire the placement preview once the resolved tree carries it.
    fn sync_nest_preview(&mut self, frame: &MapFrame<'_>) {
        if matches!(
            self.drag,
            Some(DragOp::MoveNest { .. }) | Some(DragOp::ResizeNest { .. })
        ) {
            return;
        }
        if let Some(preview) = self.nest_preview
            && let Some(nest) = frame
                .fitted_tree
                .iter()
                .find(|nest| nest.grid_id == preview.grid_id)
            && nest.i_parent_start.round() as i64 == preview.i
            && nest.j_parent_start.round() as i64 == preview.j
            && (!preview.resized || (nest.nx == preview.nx && nest.ny == preview.ny))
        {
            self.nest_preview = None;
        }
    }

    /// The path of `geom` with any previewed level's placement swapped in
    /// — a dragged parent carries its children, and the dragged nest
    /// itself paints at the preview anchor.
    fn effective_path(&self, geom: &NestGeom) -> Vec<arwen_map::NestPlacement> {
        let mut path = geom.path.clone();
        if let Some(preview) = self.nest_preview {
            for (level, id) in geom.ids.iter().enumerate() {
                if *id == preview.grid_id {
                    path[level].i_parent_start = preview.i as f64;
                    path[level].j_parent_start = preview.j as f64;
                    if preview.resized {
                        path[level].nx = preview.nx;
                        path[level].ny = preview.ny;
                    }
                }
            }
        }
        path
    }

    /// The nest's CURRENT whole-cell placement: the preview when one is
    /// live for it, else the engine's tree values.
    fn current_placement(&self, geom: &NestGeom) -> (i64, i64, u32, u32) {
        if let Some(preview) = self.nest_preview
            && preview.grid_id == geom.grid_id
        {
            return (preview.i, preview.j, preview.nx, preview.ny);
        }
        let placement = geom.placement();
        (
            placement.i_parent_start.round() as i64,
            placement.j_parent_start.round() as i64,
            placement.nx,
            placement.ny,
        )
    }

    /// Cursor position in the PARENT grid cells of nest `grid_id`
    /// (through the effective preview path).
    fn cursor_parent_cells(
        &self,
        rect: egui::Rect,
        cursor: egui::Pos2,
        frame: &MapFrame<'_>,
        grid_id: u32,
    ) -> Option<(f64, f64)> {
        let root = frame.fitted?;
        let geom = nest_geoms(frame)
            .into_iter()
            .find(|geom| geom.grid_id == grid_id)?;
        let (lon, lat) = frame.view.screen_to_lon_lat(rect, cursor);
        let (gx, gy) = root.grid_at_latlon(lat as f64, lon as f64);
        let path = self.effective_path(&geom);
        Some(arwen_map::root_grid_to_path(
            &path[..path.len() - 1],
            gx,
            gy,
        ))
    }

    /// Every fitted nest whose interior contains the cursor, SMALLEST
    /// (deepest) first — the click-cycling order.
    fn nest_candidates_at_cursor(
        &self,
        rect: egui::Rect,
        cursor: egui::Pos2,
        frame: &MapFrame<'_>,
    ) -> Vec<u32> {
        let Some(root) = frame.fitted else {
            return Vec::new();
        };
        let (lon, lat) = frame.view.screen_to_lon_lat(rect, cursor);
        let (gx, gy) = root.grid_at_latlon(lat as f64, lon as f64);
        let mut hits: Vec<(usize, u32)> = Vec::new();
        for geom in nest_geoms(frame) {
            let path = self.effective_path(&geom);
            let (x, y) = arwen_map::root_grid_to_path(&path, gx, gy);
            let deepest = path.last().expect("non-empty path");
            let inside =
                x > 0.5 && x < deepest.nx as f64 + 0.5 && y > 0.5 && y < deepest.ny as f64 + 0.5;
            if inside {
                hits.push((path.len(), geom.grid_id));
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0));
        hits.into_iter().map(|(_, grid_id)| grid_id).collect()
    }

    /// The deepest fitted nest whose interior contains the cursor.
    fn nest_at_cursor(
        &self,
        rect: egui::Rect,
        cursor: egui::Pos2,
        frame: &MapFrame<'_>,
    ) -> Option<u32> {
        self.nest_candidates_at_cursor(rect, cursor, frame)
            .first()
            .copied()
    }

    /// What a press here would grab, as a cursor icon: resize arrows on
    /// handles (selected nest first, then the root sketch), a move
    /// cursor over any nest interior, a grab hand over the root sketch.
    fn hover_cursor(
        &self,
        rect: egui::Rect,
        cursor: egui::Pos2,
        frame: &MapFrame<'_>,
    ) -> Option<egui::CursorIcon> {
        if let Some(root) = frame.fitted
            && let Some(selected) = self.selected_nest
            && let Some(geom) = nest_geoms(frame)
                .into_iter()
                .find(|geom| geom.grid_id == selected)
        {
            for (handle, position) in self.nest_handle_positions(root, &geom, frame.view, rect) {
                if position.distance(cursor) <= 7.0 {
                    return Some(handle_cursor(handle));
                }
            }
        }
        if let Some(domain) = frame.draft.domain.as_ref() {
            for (handle, position) in handle_positions(domain, frame.view, rect) {
                if position.distance(cursor) <= 7.0 {
                    return Some(handle_cursor(handle));
                }
            }
        }
        if self.nest_at_cursor(rect, cursor, frame).is_some() {
            return Some(egui::CursorIcon::Move);
        }
        if self.hit_test_handle(rect, cursor, frame) == Some(Handle::Interior) {
            return Some(egui::CursorIcon::Grab);
        }
        None
    }

    /// Drop the live placement preview (the app calls this when a queued
    /// drop was rescaled into a fresh config — the preview's cells no
    /// longer speak the config's grid).
    pub fn clear_nest_preview(&mut self) {
        self.nest_preview = None;
    }

    /// A nest gesture at this cursor, if any: resize handle of the
    /// selected nest first, then the deepest nest interior (which also
    /// selects it).
    fn nest_drag_op(
        &mut self,
        rect: egui::Rect,
        cursor: egui::Pos2,
        frame: &MapFrame<'_>,
    ) -> Option<DragOp> {
        let root = frame.fitted?;
        let geoms = nest_geoms(frame);
        if let Some(selected) = self.selected_nest
            && let Some(geom) = geoms.iter().find(|geom| geom.grid_id == selected)
        {
            for (handle, position) in self.nest_handle_positions(root, geom, frame.view, rect) {
                if position.distance(cursor) <= 7.0 {
                    return Some(DragOp::ResizeNest {
                        grid_id: selected,
                        handle,
                    });
                }
            }
        }
        let grid_id = self.nest_at_cursor(rect, cursor, frame)?;
        self.selected_nest = Some(grid_id);
        let geom = geoms.iter().find(|geom| geom.grid_id == grid_id)?;
        let (px, py) = self.cursor_parent_cells(rect, cursor, frame, grid_id)?;
        let (i, j, _, _) = self.current_placement(geom);
        Some(DragOp::MoveNest {
            grid_id,
            grab_offset: (px - i as f64, py - j as f64),
        })
    }

    /// Corner + edge-midpoint handles of a fitted nest, projected.
    fn nest_handle_positions(
        &self,
        root: &LambertDomain,
        geom: &NestGeom,
        view: &MapView,
        rect: egui::Rect,
    ) -> Vec<(Handle, egui::Pos2)> {
        let path = self.effective_path(geom);
        let deepest = *path.last().expect("non-empty path");
        let x1 = deepest.nx as f64 + 0.5;
        let y1 = deepest.ny as f64 + 0.5;
        let xm = (0.5 + x1) / 2.0;
        let ym = (0.5 + y1) / 2.0;
        let spots: [(Handle, f64, f64); 8] = [
            (Handle::Corner(0), 0.5, 0.5),
            (Handle::Corner(1), x1, 0.5),
            (Handle::Corner(2), x1, y1),
            (Handle::Corner(3), 0.5, y1),
            (Handle::Edge(0), xm, 0.5),
            (Handle::Edge(1), x1, ym),
            (Handle::Edge(2), xm, y1),
            (Handle::Edge(3), 0.5, ym),
        ];
        spots
            .into_iter()
            .map(|(handle, x, y)| {
                let (gx, gy) = arwen_map::path_grid_to_root(&path, x, y);
                let (lat, lon) = root.latlon_at_grid(gx, gy);
                (handle, view.lon_lat_to_screen(rect, lon as f32, lat as f32))
            })
            .collect()
    }

    /// Snapped, clamped MOVE: whole parent cells, kept out of the
    /// engine-reported boundary clearance. UX-only — the engine's
    /// `--resolve` remains the placement judge.
    fn drag_move_nest(
        &mut self,
        rect: egui::Rect,
        cursor: egui::Pos2,
        frame: &MapFrame<'_>,
        grid_id: u32,
        grab_offset: (f64, f64),
    ) {
        let Some(geom) = nest_geoms(frame)
            .into_iter()
            .find(|geom| geom.grid_id == grid_id)
        else {
            return;
        };
        let Some((px, py)) = self.cursor_parent_cells(rect, cursor, frame, grid_id) else {
            return;
        };
        let (_, _, nx, ny) = self.current_placement(&geom);
        let ratio = geom.ratio();
        let clearance = frame.clearance_rows.unwrap_or(0.0).max(0.0);
        let clamp_axis = |value: f64, span_points: u32, parent_n: u32| -> i64 {
            let lo = (1.0 + clearance).ceil() as i64;
            let hi =
                ((parent_n as f64) - clearance - (span_points as f64 - 1.0) / ratio).floor() as i64;
            (value.round() as i64).clamp(lo, hi.max(lo))
        };
        let i = clamp_axis(px - grab_offset.0, nx, geom.parent_nx);
        let j = clamp_axis(py - grab_offset.1, ny, geom.parent_ny);
        let resized = self
            .nest_preview
            .map(|preview| preview.grid_id == grid_id && preview.resized)
            .unwrap_or(false);
        self.nest_preview = Some(NestPreview {
            grid_id,
            i,
            j,
            nx,
            ny,
            resized,
        });
    }

    /// Snapped, clamped RESIZE: the dragged edge follows the cursor in
    /// whole parent cells; the opposite edge stays anchored; spans keep
    /// the engine's minimum and the boundary clearance.
    fn drag_resize_nest(
        &mut self,
        rect: egui::Rect,
        cursor: egui::Pos2,
        frame: &MapFrame<'_>,
        grid_id: u32,
        handle: Handle,
    ) {
        let Some(geom) = nest_geoms(frame)
            .into_iter()
            .find(|geom| geom.grid_id == grid_id)
        else {
            return;
        };
        let Some((px, py)) = self.cursor_parent_cells(rect, cursor, frame, grid_id) else {
            return;
        };
        let (i, j, nx, ny) = self.current_placement(&geom);
        let ratio = geom.ratio();
        let clearance = frame.clearance_rows.unwrap_or(0.0).max(0.0);
        // Whole-parent-cell spans (the engine's own emissions are
        // ratio-multiples: 408 = 4·102).
        let min_cells = ((frame.min_span_points.unwrap_or(2) as f64) / ratio).ceil() as i64;
        let min_cells = min_cells.max(1);
        let (mut x_lo, mut y_lo) = (i, j);
        let mut x_hi = i + ((nx as f64) / ratio).round().max(1.0) as i64;
        let mut y_hi = j + ((ny as f64) / ratio).round().max(1.0) as i64;
        let lo_bound = (1.0 + clearance).ceil() as i64;
        let x_hi_bound = ((geom.parent_nx as f64) - clearance + 1.0 / ratio).floor() as i64;
        let y_hi_bound = ((geom.parent_ny as f64) - clearance + 1.0 / ratio).floor() as i64;
        let (west, east, south, north) = match handle {
            Handle::Corner(0) => (true, false, true, false),
            Handle::Corner(1) => (false, true, true, false),
            Handle::Corner(2) => (false, true, false, true),
            Handle::Corner(3) => (true, false, false, true),
            Handle::Edge(0) => (false, false, true, false),
            Handle::Edge(1) => (false, true, false, false),
            Handle::Edge(2) => (false, false, false, true),
            Handle::Edge(3) => (true, false, false, false),
            Handle::Interior | Handle::Corner(_) | Handle::Edge(_) => return,
        };
        // Every clamp keeps min ≤ max even over an already-illegal
        // current placement (the engine's refusal handles legality).
        if west {
            let hi_limit = x_hi - min_cells;
            x_lo = (px.round() as i64).clamp(lo_bound.min(hi_limit), hi_limit);
        }
        if east {
            x_hi = (px.round() as i64).clamp(x_lo + min_cells, x_hi_bound.max(x_lo + min_cells));
        }
        if south {
            let hi_limit = y_hi - min_cells;
            y_lo = (py.round() as i64).clamp(lo_bound.min(hi_limit), hi_limit);
        }
        if north {
            y_hi = (py.round() as i64).clamp(y_lo + min_cells, y_hi_bound.max(y_lo + min_cells));
        }
        self.nest_preview = Some(NestPreview {
            grid_id,
            i: x_lo,
            j: y_lo,
            nx: ((x_hi - x_lo) * ratio as i64).max(1) as u32,
            ny: ((y_hi - y_lo) * ratio as i64).max(1) as u32,
            resized: true,
        });
    }

    fn hit_test_handle(
        &self,
        rect: egui::Rect,
        cursor: egui::Pos2,
        frame: &MapFrame<'_>,
    ) -> Option<Handle> {
        let domain = frame.draft.domain.as_ref()?;
        let handles = handle_positions(domain, frame.view, rect);
        for (handle, position) in &handles {
            if position.distance(cursor) <= 7.0 {
                return Some(*handle);
            }
        }
        // Interior: inside the projected outline's screen polygon (convex
        // enough at these scales for a bbox + grid-space test).
        let (gx, gy) = {
            let (lon, lat) = frame.view.screen_to_lon_lat(rect, cursor);
            domain.grid_at_latlon(lat as f64, lon as f64)
        };
        let inside =
            gx > 0.5 && gx < domain.nx as f64 + 0.5 && gy > 0.5 && gy < domain.ny as f64 + 0.5;
        inside.then_some(Handle::Interior)
    }

    fn paint_domain(&self, painter: &egui::Painter, rect: egui::Rect, frame: &MapFrame<'_>) {
        // A running/analyzed session paints its own (frozen) domain —
        // root, nests (dormant ones ghosted), and the relocation trail.
        if let Some(session) = frame.session
            && let Some(domain) = &session.domain
        {
            let points = frame
                .view
                .project_polyline(rect, domain.outline_lon_lat(24));
            painter.add(egui::Shape::line(
                points,
                egui::Stroke::new(1.6, theme().accent),
            ));
            for (grid_id, path) in nest_paths(&session.tree) {
                let outline = arwen_map::nest_outline_lon_lat(domain, &path, 24);
                let points = frame.view.project_polyline(rect, outline);
                let dormant = session
                    .tree
                    .iter()
                    .find(|nest| nest.grid_id == grid_id)
                    .and_then(|nest| nest.spawn_trigger.as_deref());
                paint_nest_outline(painter, points, grid_id, dormant, &session.tree);
            }
            paint_relocation_trail(painter, rect, frame, domain, session);
        }

        // Draw-preview rect.
        if let Some((a, b)) = self.preview {
            if let Some(domain) = domain_from_drag(a, b, frame.draft.dx_m()) {
                let points = frame
                    .view
                    .project_polyline(rect, domain.outline_lon_lat(16));
                painter.add(egui::Shape::line(
                    points,
                    egui::Stroke::new(1.2, theme().accent_text),
                ));
            }
        }

        // The engine-fitted grid from an open review or the working
        // config: the strong truth outline over the sketch — root AND
        // every fitted nest, each DIRECTLY placeable when editable.
        if frame.session.is_none()
            && let Some(fitted) = frame.fitted
        {
            let points = frame
                .view
                .project_polyline(rect, fitted.outline_lon_lat(24));
            let root_corner = points.first().copied();
            painter.add(egui::Shape::line(
                points,
                egui::Stroke::new(2.4, theme().accent_text),
            ));
            // The root carries its facts like any nest: id · dx ·
            // nx × ny, from the ENGINE's own tree values.
            if let Some(position) = root_corner
                && let Some(root) = frame
                    .fitted_tree
                    .iter()
                    .find(|domain| domain.parent_id == 0)
            {
                let dx = if root.dx_m >= 1000.0 {
                    format!("{:.0} km", root.dx_m / 1000.0)
                } else {
                    format!("{:.0} m", root.dx_m)
                };
                painter.text(
                    position + egui::vec2(4.0, -3.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!(
                        "d{:02} · {dx} · {} × {}{}",
                        root.grid_id,
                        root.nx,
                        root.ny,
                        if frame.tree_provisional {
                            " · sketch"
                        } else {
                            ""
                        }
                    ),
                    egui::FontId::proportional(10.0),
                    theme().accent_text,
                );
            }
            let geoms = nest_geoms(frame);
            // The clearance keepout the engine reports: a faint
            // forbidden band inside every PARENT's edge, so a stopped
            // drag explains itself.
            if frame.editable
                && !geoms.is_empty()
                && let Some(clearance) = frame.clearance_rows.filter(|rows| *rows > 0.0)
            {
                paint_keepout_band(painter, frame.view, rect, fitted, &[], clearance);
                for geom in &geoms {
                    if geom.has_children(frame.fitted_tree) {
                        // The inset is in this parent's OWN grid cells —
                        // the same rows the engine's floor data names.
                        paint_keepout_band(
                            painter,
                            frame.view,
                            rect,
                            fitted,
                            &self.effective_path(geom),
                            clearance,
                        );
                    }
                }
            }
            for geom in &geoms {
                let path = self.effective_path(geom);
                let deepest = *path.last().expect("non-empty path");
                let outline = arwen_map::nest_outline_lon_lat(fitted, &path, 24);
                let points = frame.view.project_polyline(rect, outline);
                let dormant = frame
                    .fitted_tree
                    .iter()
                    .find(|nest| nest.grid_id == geom.grid_id)
                    .and_then(|nest| nest.spawn_trigger.as_deref());
                let selected = frame.editable && self.selected_nest == Some(geom.grid_id);
                let hovered = frame.editable && self.hovered_nest == Some(geom.grid_id);
                // Hover: an unmistakable fill tint under the cursor.
                if hovered && !selected && points.len() > 3 {
                    painter.add(egui::Shape::convex_polygon(
                        points[..points.len() - 1].to_vec(),
                        egui::Color32::from_rgba_unmultiplied(122, 178, 222, 40),
                        egui::Stroke::NONE,
                    ));
                }
                if selected {
                    painter.add(egui::Shape::line(
                        points.clone(),
                        egui::Stroke::new(3.2, theme().accent),
                    ));
                }
                // Selected label carries the facts: id · dx · nx×ny —
                // dx is the ENGINE's own number from the tree.
                let detail = if selected {
                    let dx_m = frame
                        .fitted_tree
                        .iter()
                        .find(|nest| nest.grid_id == geom.grid_id)
                        .map(|nest| nest.dx_m)
                        .unwrap_or(0.0);
                    let dx = if dx_m >= 1000.0 {
                        format!("{:.0} km", dx_m / 1000.0)
                    } else {
                        format!("{dx_m:.0} m")
                    };
                    Some(format!(
                        "d{:02} · {dx} · {} × {}{}",
                        geom.grid_id,
                        deepest.nx,
                        deepest.ny,
                        if frame.tree_provisional {
                            " · sketch"
                        } else {
                            ""
                        }
                    ))
                } else if frame.tree_provisional {
                    Some(format!(
                        "d{:02} · sketch (engine fits it on the fly)",
                        geom.grid_id
                    ))
                } else {
                    None
                };
                paint_nest_outline_styled(
                    painter,
                    points,
                    geom.grid_id,
                    dormant,
                    frame.fitted_tree,
                    frame.tree_provisional,
                    detail.as_deref(),
                );
                if selected {
                    for (_, position) in self.nest_handle_positions(fitted, geom, frame.view, rect)
                    {
                        painter.rect_filled(
                            egui::Rect::from_center_size(position, egui::vec2(7.0, 7.0)),
                            1.0,
                            theme().accent,
                        );
                        painter.rect_stroke(
                            egui::Rect::from_center_size(position, egui::vec2(7.0, 7.0)),
                            1.0,
                            egui::Stroke::new(1.0, theme().inset),
                            egui::StrokeKind::Outside,
                        );
                    }
                }
                // The live placement while dragging / re-resolving:
                // whole-cell anchor in the parent, stated at the rect.
                if let Some(preview) = self.nest_preview.filter(|p| p.grid_id == geom.grid_id) {
                    let (gx, gy) = arwen_map::path_grid_to_root(&path, 1.0, 1.0);
                    let (lat, lon) = fitted.latlon_at_grid(gx, gy);
                    let anchor = frame.view.lon_lat_to_screen(rect, lon as f32, lat as f32);
                    painter.text(
                        anchor + egui::vec2(4.0, 14.0),
                        egui::Align2::LEFT_TOP,
                        format!(
                            "→ anchored ({}, {}) · {} × {}",
                            preview.i, preview.j, preview.nx, preview.ny
                        ),
                        egui::FontId::proportional(10.0),
                        theme().accent,
                    );
                }
            }
            // Spawn-trigger search boxes (declared + the live drag),
            // in the spawning nest's parent cells.
            for (grid_id, bounds) in &frame.search_boxes {
                if self
                    .search_preview
                    .map(|(_, preview_grid)| preview_grid == *grid_id)
                    .unwrap_or(false)
                {
                    continue; // the live drag repaints this one
                }
                self.paint_search_box(painter, frame, rect, fitted, *grid_id, *bounds, false);
            }
            if let Some((bounds, grid_id)) = self.search_preview {
                self.paint_search_box(painter, frame, rect, fitted, grid_id, bounds, true);
            }
            // Domains the engine's refusal NAMES: a red outline over the
            // offender with the engine's own sentence anchored to it —
            // never a bare "estimate failed" pointing at nothing.
            for (grid_id, sentence) in &frame.refusal_domains {
                let is_root = frame
                    .fitted_tree
                    .iter()
                    .find(|domain| domain.grid_id == *grid_id)
                    .map(|domain| domain.parent_id == 0)
                    .unwrap_or(false);
                let points = if is_root {
                    frame
                        .view
                        .project_polyline(rect, fitted.outline_lon_lat(24))
                } else if let Some(geom) = geoms.iter().find(|geom| geom.grid_id == *grid_id) {
                    let path = self.effective_path(geom);
                    frame
                        .view
                        .project_polyline(rect, arwen_map::nest_outline_lon_lat(fitted, &path, 24))
                } else {
                    continue;
                };
                let corner = points.first().copied();
                painter.add(egui::Shape::line(
                    points,
                    egui::Stroke::new(2.6, theme().alert),
                ));
                if let Some(position) = corner {
                    let galley = painter.layout(
                        sentence.clone(),
                        egui::FontId::proportional(10.0),
                        theme().alert,
                        320.0,
                    );
                    let padding = egui::vec2(6.0, 4.0);
                    let card = egui::Rect::from_min_size(
                        position + egui::vec2(4.0, 16.0),
                        galley.size() + padding * 2.0,
                    );
                    painter.rect_filled(
                        card,
                        3.0,
                        egui::Color32::from_rgba_unmultiplied(15, 14, 13, 220),
                    );
                    painter.galley(card.min + padding, galley, theme().alert);
                }
            }
        }

        // The draft domain with its handles.
        if frame.session.is_none()
            && let Some(domain) = &frame.draft.domain
        {
            let points = frame
                .view
                .project_polyline(rect, domain.outline_lon_lat(24));
            painter.add(egui::Shape::line(
                points,
                egui::Stroke::new(1.8, theme().accent),
            ));
            if frame.editable {
                for (_, position) in handle_positions(domain, frame.view, rect) {
                    painter.rect_filled(
                        egui::Rect::from_center_size(position, egui::vec2(7.0, 7.0)),
                        1.0,
                        theme().accent,
                    );
                    painter.rect_stroke(
                        egui::Rect::from_center_size(position, egui::vec2(7.0, 7.0)),
                        1.0,
                        egui::Stroke::new(1.0, theme().inset),
                        egui::StrokeKind::Outside,
                    );
                }
            }
        }
    }

    /// One spawn search box: a dashed rect over the spawning nest's
    /// PARENT cells (1-based inclusive bounds), warn-colored; the live
    /// drag paints stronger.
    #[allow(clippy::too_many_arguments)]
    fn paint_search_box(
        &self,
        painter: &egui::Painter,
        frame: &MapFrame<'_>,
        rect: egui::Rect,
        root: &LambertDomain,
        grid_id: u32,
        bounds: [i64; 4],
        live: bool,
    ) {
        let Some(geom) = nest_geoms(frame)
            .into_iter()
            .find(|geom| geom.grid_id == grid_id)
        else {
            return;
        };
        let path = self.effective_path(&geom);
        let parent_path = &path[..path.len() - 1];
        let (x0, y0) = (bounds[0] as f64 - 0.5, bounds[1] as f64 - 0.5);
        let (x1, y1) = (bounds[2] as f64 + 0.5, bounds[3] as f64 + 0.5);
        let samples = 9;
        let mut ring: Vec<(f64, f64)> = Vec::with_capacity(samples * 4 + 1);
        let mut push = |x: f64, y: f64| {
            let (gx, gy) = arwen_map::path_grid_to_root(parent_path, x, y);
            let (lat, lon) = root.latlon_at_grid(gx, gy);
            ring.push((lon, lat));
        };
        for index in 0..samples {
            let t = index as f64 / (samples - 1) as f64;
            push(x0 + (x1 - x0) * t, y0);
        }
        for index in 1..samples {
            let t = index as f64 / (samples - 1) as f64;
            push(x1, y0 + (y1 - y0) * t);
        }
        for index in 1..samples {
            let t = index as f64 / (samples - 1) as f64;
            push(x1 - (x1 - x0) * t, y1);
        }
        for index in 1..samples {
            let t = index as f64 / (samples - 1) as f64;
            push(x0, y1 - (y1 - y0) * t);
        }
        if let Some(first) = ring.first().copied() {
            ring.push(first);
        }
        let points = frame.view.project_polyline(rect, ring);
        let stroke = if live {
            egui::Stroke::new(1.8, theme().warn)
        } else {
            egui::Stroke::new(
                1.2,
                egui::Color32::from_rgba_unmultiplied(224, 164, 84, 150),
            )
        };
        let corner = points.first().copied();
        painter.extend(egui::Shape::dashed_line(&points, stroke, 5.0, 4.0));
        if let Some(position) = corner {
            painter.text(
                position + egui::vec2(4.0, -3.0),
                egui::Align2::LEFT_BOTTOM,
                format!(
                    "spawn search d{grid_id:02} [{}, {}, {}, {}]",
                    bounds[0], bounds[1], bounds[2], bounds[3]
                ),
                egui::FontId::proportional(9.5),
                stroke.color,
            );
        }
    }

    /// The engine's refusal of the current placement, verbatim, at the
    /// canvas foot — the reason a drag was stopped or a config refused.
    fn paint_placement_refusal(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        frame: &MapFrame<'_>,
    ) {
        let Some(refusal) = &frame.placement_refusal else {
            return;
        };
        painter.text(
            egui::pos2(rect.center().x, rect.bottom() - 42.0),
            egui::Align2::CENTER_BOTTOM,
            refusal,
            egui::FontId::proportional(11.0),
            theme().alert,
        );
    }

    fn paint_marker(&self, painter: &egui::Painter, rect: egui::Rect, frame: &MapFrame<'_>) {
        let Some(marker) = &self.marker else { return };
        let position = frame.view.lon_lat_to_screen(rect, marker.lon, marker.lat);
        if !rect.contains(position) {
            return;
        }
        painter.circle_stroke(position, 6.0, egui::Stroke::new(1.5, theme().accent_text));
        painter.circle_filled(position, 2.0, theme().accent_text);
        if let Some(label) = &marker.label {
            painter.text(
                position + egui::vec2(9.0, -9.0),
                egui::Align2::LEFT_BOTTOM,
                label,
                egui::FontId::proportional(11.0),
                theme().text,
            );
        }
    }

    /// AWIPS-style hover readout (chrome_readouts pattern from BowEcho):
    /// real numbers, bottom-left of the canvas.
    fn paint_hover_readout(
        &self,
        ui: &egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        frame: &MapFrame<'_>,
    ) {
        let Some(cursor) = response.hover_pos() else {
            return;
        };
        let (lon, lat) = frame.view.screen_to_lon_lat(rect, cursor);
        let mut parts = vec![format!(
            "{:.3}{} {:.3}{}",
            lat.abs(),
            if lat < 0.0 { "S" } else { "N" },
            lon.abs(),
            if lon < 0.0 { "W" } else { "E" },
        )];
        if let Some(context) = search::country_context_for_lon_lat(lat, lon) {
            parts.push(context.to_string());
        }
        let domain = frame
            .session
            .and_then(|session| session.domain.as_ref())
            .or(frame.draft.domain.as_ref());
        if let Some(domain) = domain {
            let (gx, gy) = domain.grid_at_latlon(lat as f64, lon as f64);
            if gx >= 0.5
                && gx <= domain.nx as f64 + 0.5
                && gy >= 0.5
                && gy <= domain.ny as f64 + 0.5
            {
                parts.push(format!("grid {:.0},{:.0}", gx.round(), gy.round()));
            }
        }
        if let Some((field, _)) = frame.model.and_then(|model| model.loaded.as_ref())
            && let Some(value) = field.value_at(lat, lon)
        {
            parts.push(format!(
                "{} {value:.1} {}",
                field.name.to_uppercase(),
                field.units
            ));
        }
        let text = parts.join("  ·  ");
        let galley = ui.painter().layout_no_wrap(
            text,
            egui::FontId::monospace(crate::theme::READOUT_FONT_SIZE),
            theme().text,
        );
        let padding = egui::vec2(8.0, 4.0);
        let card = egui::Rect::from_min_size(
            egui::pos2(
                rect.left() + 8.0,
                rect.bottom() - galley.size().y - padding.y * 2.0 - 8.0,
            ),
            galley.size() + padding * 2.0,
        );
        let painter = ui.painter();
        painter.rect_filled(
            card,
            3.0,
            egui::Color32::from_rgba_unmultiplied(15, 14, 13, 215),
        );
        painter.galley(card.min + padding, galley, theme().text);
    }

    fn paint_attribution(&self, painter: &egui::Painter, rect: egui::Rect, frame: &MapFrame<'_>) {
        if let Some(attribution) = frame.tile_style.attribution() {
            painter.text(
                egui::pos2(rect.right() - 6.0, rect.bottom() - 4.0),
                egui::Align2::RIGHT_BOTTOM,
                attribution,
                egui::FontId::proportional(9.0),
                egui::Color32::from_rgba_unmultiplied(230, 234, 238, 140),
            );
        }
    }

    /// The floating search overlay (top-center of the canvas) plus the
    /// draw-domain toggle. Called by the app AFTER the canvas so the
    /// overlay sits on top.
    pub fn overlay_ui(&mut self, ui: &mut egui::Ui, rect: egui::Rect, frame: &mut MapFrame<'_>) {
        let overlay_width = 340.0_f32.min(rect.width() - 24.0);
        // Anchored at the canvas top; room grows DOWNWARD only (a
        // symmetric expand once floated the search box off-screen above —
        // caught by the synthesized-input test).
        let overlay_rect = egui::Rect::from_min_size(
            egui::pos2(rect.center().x - overlay_width / 2.0, rect.top() + 10.0),
            egui::vec2(overlay_width, 240.0),
        );

        let empty_state = frame.editable && frame.draft.domain.is_none() && frame.session.is_none();
        if empty_state && !self.draw_mode {
            // The empty state: a subdued invitation, never a wizard.
            ui.painter().text(
                egui::pos2(rect.center().x, rect.center().y - 10.0),
                egui::Align2::CENTER_CENTER,
                "Search for a place, then draw your forecast domain",
                egui::FontId::proportional(15.0),
                theme().text_weak,
            );
        }

        let builder = egui::UiBuilder::new()
            .max_rect(overlay_rect)
            .layout(egui::Layout::top_down(egui::Align::Min));
        let mut child = ui.new_child(builder);
        child.horizontal(|ui| {
            ui.set_width(overlay_width);
            let hint = "Search place or lat, lon";
            let edit = egui::TextEdit::singleline(&mut self.search_query)
                .hint_text(hint)
                .desired_width(overlay_width - 110.0);
            let response = ui.add(edit);
            self.last_search_rect = Some(response.rect);
            let submitted =
                response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            if response.changed() {
                self.search_error = None;
                self.search_results = search::place_search_matches(
                    &self.search_query,
                    frame.view.center_lat,
                    frame.view.center_lon,
                    8,
                );
            }
            if submitted {
                self.submit_search(frame);
            }
            if frame.editable {
                let has_domain = frame.draft.domain.is_some();
                let label = if self.draw_mode {
                    "Drawing… (Esc)"
                } else if has_domain {
                    "Redraw domain"
                } else {
                    "Draw domain"
                };
                let button = egui::Button::selectable(self.draw_mode, label);
                if ui
                    .add(button)
                    .on_hover_text(if has_domain {
                        "Replace the drawn parent with a new rectangle (D). \
                         Clicks on the map otherwise SELECT domains — drawing \
                         is a mode you arm."
                    } else {
                        "Drag a rectangle on the map to place the forecast domain (D)"
                    })
                    .clicked()
                {
                    self.request_draw_mode(has_domain);
                }
            }
        });
        if self.confirm_redraw {
            child.horizontal(|ui| {
                ui.colored_label(
                    theme().warn,
                    "Replace the drawn parent? The config regenerates for the \
                     new rectangle — children and knob edits reset with it.",
                );
                if ui.button("Replace").clicked() {
                    self.confirm_redraw = false;
                    self.draw_mode = true;
                }
                if ui.button("Keep").clicked() {
                    self.confirm_redraw = false;
                }
            });
        }
        if let Some(error) = &self.search_error {
            child.colored_label(theme().alert, error);
        }
        if !self.search_results.is_empty() && !self.search_query.is_empty() {
            egui::Frame::new()
                .fill(theme().raised)
                .stroke(egui::Stroke::new(1.0, theme().outline))
                .corner_radius(3.0)
                .show(&mut child, |ui| {
                    ui.set_width(overlay_width);
                    let picked: Vec<PlaceSearchResult> = self.search_results.clone();
                    for result in picked {
                        let response = ui.add(
                            egui::Button::new(
                                egui::RichText::new(result.display_label()).size(12.0),
                            )
                            .frame(false)
                            .min_size(egui::vec2(overlay_width - 12.0, 20.0)),
                        );
                        if response.clicked() {
                            self.jump_to(
                                frame,
                                result.lon,
                                result.lat,
                                Some(result.display_label()),
                            );
                            self.search_results.clear();
                            self.search_query.clear();
                        }
                    }
                });
        }

        // Keyboard: D toggles draw mode when the canvas has focus space.
        if frame.editable
            && ui.input(|input| input.key_pressed(egui::Key::D) && input.modifiers.is_none())
            && !ui.ctx().egui_wants_keyboard_input()
        {
            if self.draw_mode {
                self.draw_mode = false;
            } else {
                self.request_draw_mode(frame.draft.domain.is_some());
            }
        }
    }

    /// Arm draw mode — through the replace-confirm when a parent already
    /// exists (a completed redraw REPLACES it and resets children).
    pub fn request_draw_mode(&mut self, has_domain: bool) {
        if self.draw_mode {
            self.draw_mode = false;
        } else if has_domain {
            self.confirm_redraw = true;
        } else {
            self.draw_mode = true;
        }
    }

    fn submit_search(&mut self, frame: &mut MapFrame<'_>) {
        match search::parse_coordinate_search_query(&self.search_query) {
            Ok(Some(marker)) => {
                let (lon, lat, label) = (marker.lon, marker.lat, marker.label.clone());
                self.marker = Some(marker);
                self.jump_to(frame, lon, lat, label);
                self.search_results.clear();
                return;
            }
            Ok(None) => {}
            Err(error) => {
                self.search_error = Some(error);
                return;
            }
        }
        if let Some(result) = self.search_results.first().copied() {
            self.jump_to(frame, result.lon, result.lat, Some(result.display_label()));
            self.search_results.clear();
            self.search_query.clear();
        }
    }

    fn jump_to(&mut self, frame: &mut MapFrame<'_>, lon: f32, lat: f32, label: Option<String>) {
        frame.view.center_lon = lon;
        frame.view.center_lat = lat;
        frame.view.clamp_center();
        frame.view.scale = frame.view.scale.max(40.0);
        self.marker = Some(CoordinateMarker { lat, lon, label });
    }
}

/// The engine-reported boundary-clearance keepout: a faint filled band
/// between a parent's edge and the same outline inset by
/// `clearance_rows` of its own grid, plus a dashed inner ring — the
/// region a child may not enter, painted so a stopped drag explains
/// itself. Geometry only; legality stays the engine's answer.
fn paint_keepout_band(
    painter: &egui::Painter,
    view: &MapView,
    rect: egui::Rect,
    root: &LambertDomain,
    path: &[arwen_map::NestPlacement],
    clearance_rows: f64,
) {
    let outer = view.project_polyline(rect, arwen_map::nest_ring_lon_lat(root, path, 0.0, 24));
    let inner = view.project_polyline(
        rect,
        arwen_map::nest_ring_lon_lat(root, path, clearance_rows, 24),
    );
    if outer.len() != inner.len() || outer.len() < 2 {
        return;
    }
    let fill = egui::Color32::from_rgba_unmultiplied(216, 96, 80, 40);
    let mut mesh = egui::epaint::Mesh::default();
    for (outer_point, inner_point) in outer.iter().zip(&inner) {
        mesh.colored_vertex(*outer_point, fill);
        mesh.colored_vertex(*inner_point, fill);
    }
    for index in 0..(outer.len() as u32 - 1) {
        let a = index * 2;
        mesh.add_triangle(a, a + 1, a + 2);
        mesh.add_triangle(a + 1, a + 3, a + 2);
    }
    painter.add(egui::Shape::mesh(mesh));
    painter.extend(egui::Shape::dashed_line(
        &inner,
        egui::Stroke::new(
            1.2,
            egui::Color32::from_rgba_unmultiplied(226, 110, 92, 150),
        ),
        4.0,
        5.0,
    ));
}

/// One nest outline: solid for live domains, GHOSTED (dashed, reduced
/// alpha) with a trigger badge for dormant spawn declarations — the
/// END-GAME §2.4 pattern.
fn paint_nest_outline(
    painter: &egui::Painter,
    points: Vec<egui::Pos2>,
    grid_id: u32,
    dormant_trigger: Option<&str>,
    tree: &[arwen_plan::queries::ResolvedDomain],
) {
    paint_nest_outline_styled(painter, points, grid_id, dormant_trigger, tree, false, None);
}

/// [`paint_nest_outline`] with the design-view extras: `provisional`
/// (draft placeholder — dashed, still interactive) and an optional
/// richer label (the selected nest's id · dx · size).
fn paint_nest_outline_styled(
    painter: &egui::Painter,
    points: Vec<egui::Pos2>,
    grid_id: u32,
    dormant_trigger: Option<&str>,
    tree: &[arwen_plan::queries::ResolvedDomain],
    provisional: bool,
    detail: Option<&str>,
) {
    let corner = points.first().copied();
    match dormant_trigger {
        None => {
            if provisional {
                painter.extend(egui::Shape::dashed_line(
                    &points,
                    egui::Stroke::new(1.6, theme().accent_text),
                    7.0,
                    4.0,
                ));
            } else {
                painter.add(egui::Shape::line(
                    points,
                    egui::Stroke::new(1.8, theme().accent_text),
                ));
            }
            if let Some(position) = corner {
                painter.text(
                    position + egui::vec2(4.0, 2.0),
                    egui::Align2::LEFT_TOP,
                    detail
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("d{grid_id:02}")),
                    egui::FontId::proportional(10.0),
                    theme().accent_text,
                );
            }
        }
        Some(trigger) => {
            let ghost = egui::Color32::from_rgba_unmultiplied(163, 205, 232, 110);
            painter.extend(egui::Shape::dashed_line(
                &points,
                egui::Stroke::new(1.4, ghost),
                6.0,
                5.0,
            ));
            if let Some(position) = corner {
                let nest = tree.iter().find(|nest| nest.grid_id == grid_id);
                let badge = crate::storm::spawn_badge(
                    trigger,
                    nest.and_then(|nest| nest.spawn_threshold),
                    nest.and_then(|nest| nest.spawn_at_s),
                );
                painter.text(
                    position + egui::vec2(4.0, 2.0),
                    egui::Align2::LEFT_TOP,
                    format!("d{grid_id:02} · dormant · {badge}"),
                    egui::FontId::proportional(10.0),
                    ghost,
                );
            }
        }
    }
}

/// The relocation trail (END-GAME §2.3): breadcrumb centers of every
/// executed move from the runner's own receipts, replayed on the map.
fn paint_relocation_trail(
    painter: &egui::Painter,
    rect: egui::Rect,
    frame: &MapFrame<'_>,
    root: &LambertDomain,
    session: &RunSession,
) {
    if session.trail.is_empty() {
        return;
    }
    let mut centers: Vec<(egui::Pos2, f64)> = Vec::new();
    for (index, movement) in session.trail.iter().enumerate() {
        if index == 0
            && let Some(center) =
                nest_center_lon_lat(root, &session.tree, movement.grid_id, movement.from)
        {
            let position = frame
                .view
                .lon_lat_to_screen(rect, center.0 as f32, center.1 as f32);
            centers.push((position, 0.0));
        }
        if let Some(center) =
            nest_center_lon_lat(root, &session.tree, movement.grid_id, movement.to)
        {
            let position = frame
                .view
                .lon_lat_to_screen(rect, center.0 as f32, center.1 as f32);
            centers.push((position, movement.elapsed_seconds));
        }
    }
    if centers.len() < 2 {
        return;
    }
    let line: Vec<egui::Pos2> = centers.iter().map(|(position, _)| *position).collect();
    painter.add(egui::Shape::line(
        line,
        egui::Stroke::new(1.2, theme().warn),
    ));
    for (position, elapsed) in &centers {
        painter.circle_filled(*position, 2.5, theme().warn);
        if *elapsed > 0.0 {
            painter.text(
                *position + egui::vec2(5.0, -5.0),
                egui::Align2::LEFT_BOTTOM,
                crate::kit::format_lead_hours(*elapsed),
                egui::FontId::proportional(9.0),
                theme().warn,
            );
        }
    }
}

/// The child-CENTER (lon, lat) of `grid_id` if its placement were
/// `(i_parent_start, j_parent_start)` — the trail's replay geometry,
/// built from the ENGINE's own tree values.
fn nest_center_lon_lat(
    root: &LambertDomain,
    tree: &[arwen_plan::queries::ResolvedDomain],
    grid_id: u32,
    placement: (f64, f64),
) -> Option<(f64, f64)> {
    let nest = tree.iter().find(|nest| nest.grid_id == grid_id)?;
    let mut path = nest_paths(tree)
        .into_iter()
        .find(|(id, _)| *id == grid_id)?
        .1;
    if let Some(last) = path.last_mut() {
        last.i_parent_start = placement.0;
        last.j_parent_start = placement.1;
    }
    let (mut gx, mut gy) = ((nest.nx as f64 + 1.0) / 2.0, (nest.ny as f64 + 1.0) / 2.0);
    for level in path.iter().rev() {
        gx = level.i_parent_start + (gx - 1.0) / level.parent_grid_ratio;
        gy = level.j_parent_start + (gy - 1.0) / level.parent_grid_ratio;
    }
    let (lat, lon) = root.latlon_at_grid(gx, gy);
    Some((lon, lat))
}

/// The placement chain (root → nest) for every non-root domain of a
/// resolved tree, with its grid_id. Placements are the ENGINE's own
/// `[[domain]]` values; a parent link that resolves to nothing is
/// skipped rather than guessed.
fn nest_paths(
    tree: &[arwen_plan::queries::ResolvedDomain],
) -> Vec<(u32, Vec<arwen_map::NestPlacement>)> {
    tree.iter()
        .filter(|domain| domain.parent_id != 0)
        .filter_map(|nest| {
            let mut path = Vec::new();
            let mut current = nest.clone();
            loop {
                path.push(arwen_map::NestPlacement {
                    i_parent_start: current.i_parent_start,
                    j_parent_start: current.j_parent_start,
                    parent_grid_ratio: current.parent_grid_ratio,
                    nx: current.nx,
                    ny: current.ny,
                });
                let parent = tree
                    .iter()
                    .find(|domain| domain.grid_id == current.parent_id)?;
                if parent.parent_id == 0 {
                    break;
                }
                current = parent.clone();
            }
            path.reverse();
            Some((nest.grid_id, path))
        })
        .collect()
}

/// The resize/move cursor for a handle (screen orientation: grid corner
/// 0 = SW = bottom-left).
fn handle_cursor(handle: Handle) -> egui::CursorIcon {
    match handle {
        Handle::Corner(0) | Handle::Corner(2) => egui::CursorIcon::ResizeNeSw,
        Handle::Corner(_) => egui::CursorIcon::ResizeNwSe,
        Handle::Edge(0) | Handle::Edge(2) => egui::CursorIcon::ResizeVertical,
        Handle::Edge(_) => egui::CursorIcon::ResizeHorizontal,
        Handle::Interior => egui::CursorIcon::Move,
    }
}

/// Handles: 4 corners + 4 edge midpoints, projected.
fn handle_positions(
    domain: &LambertDomain,
    view: &MapView,
    rect: egui::Rect,
) -> Vec<(Handle, egui::Pos2)> {
    let x1 = domain.nx as f64 + 0.5;
    let y1 = domain.ny as f64 + 0.5;
    let xm = (0.5 + x1) / 2.0;
    let ym = (0.5 + y1) / 2.0;
    let spots: [(Handle, f64, f64); 8] = [
        (Handle::Corner(0), 0.5, 0.5),
        (Handle::Corner(1), x1, 0.5),
        (Handle::Corner(2), x1, y1),
        (Handle::Corner(3), 0.5, y1),
        (Handle::Edge(0), xm, 0.5),
        (Handle::Edge(1), x1, ym),
        (Handle::Edge(2), xm, y1),
        (Handle::Edge(3), 0.5, ym),
    ];
    spots
        .into_iter()
        .map(|(handle, gx, gy)| {
            let (lat, lon) = domain.latlon_at_grid(gx, gy);
            (handle, view.lon_lat_to_screen(rect, lon as f32, lat as f32))
        })
        .collect()
}

/// Build a domain from a drawn geo rect: center at the midpoint, extent
/// from great-circle distances, grid counts snapped to dx. (pub(crate):
/// the livefire driver drives this exact function in place of the mouse.)
pub(crate) fn domain_from_drag(a: (f64, f64), b: (f64, f64), dx_m: f64) -> Option<LambertDomain> {
    let center_lon = (a.0 + b.0) / 2.0;
    let center_lat = (a.1 + b.1) / 2.0;
    let width_km =
        arwen_map::geo::haversine_km(center_lat as f32, a.0 as f32, center_lat as f32, b.0 as f32)
            as f64;
    let height_km =
        arwen_map::geo::haversine_km(a.1 as f32, center_lon as f32, b.1 as f32, center_lon as f32)
            as f64;
    let nx = ((width_km * 1000.0 / dx_m).round() as u32).max(16);
    let ny = ((height_km * 1000.0 / dx_m).round() as u32).max(16);
    if width_km < 20.0 || height_km < 20.0 {
        return None; // an accidental click, not a domain
    }
    let domain = LambertDomain::centered_at(center_lat, center_lon, nx, ny, dx_m);
    domain.validate().ok()?;
    Some(domain)
}

/// Resize in grid space: the dragged handle follows the cursor's grid
/// coordinate, the opposite edge/corner stays fixed, and the domain is
/// rebuilt centered on the new grid midpoint.
fn resize_domain(domain: &mut LambertDomain, handle: Handle, lat: f64, lon: f64) {
    let (gx, gy) = domain.grid_at_latlon(lat, lon);
    let (west, east, south, north) = (0.5, domain.nx as f64 + 0.5, 0.5, domain.ny as f64 + 0.5);
    let (new_west, new_east, new_south, new_north) = match handle {
        Handle::Corner(0) => (gx, east, gy, north),
        Handle::Corner(1) => (west, gx, gy, north),
        Handle::Corner(2) => (west, gx, south, gy),
        Handle::Corner(3) => (gx, east, south, gy),
        Handle::Edge(0) => (west, east, gy, north),
        Handle::Edge(1) => (west, gx, south, north),
        Handle::Edge(2) => (west, east, south, gy),
        Handle::Edge(3) => (gx, east, south, north),
        Handle::Interior | Handle::Corner(_) | Handle::Edge(_) => return,
    };
    let nx = ((new_east - new_west).round() as i64).max(16) as u32;
    let ny = ((new_north - new_south).round() as i64).max(16) as u32;
    let mid_x = (new_west + new_east) / 2.0;
    let mid_y = (new_south + new_north) / 2.0;
    let (ref_lat, ref_lon) = domain.latlon_at_grid(mid_x, mid_y);
    *domain = LambertDomain {
        nx,
        ny,
        ref_lat,
        ref_lon,
        stand_lon: ref_lon,
        ..*domain
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_builds_a_snapped_domain_and_tiny_drags_do_not() {
        let domain = domain_from_drag((-99.0, 34.0), (-95.0, 37.0), 3_000.0).unwrap();
        assert!((domain.ref_lon + 97.0).abs() < 1e-9);
        assert!((domain.ref_lat - 35.5).abs() < 1e-9);
        // ~4° lon at 35.5N ≈ 362 km → ~121 cells at 3 km.
        assert!((100..=140).contains(&domain.nx), "{}", domain.nx);
        assert!((100..=125).contains(&domain.ny), "{}", domain.ny);

        assert!(domain_from_drag((-97.0, 35.0), (-96.99, 35.01), 3_000.0).is_none());
    }

    #[test]
    fn east_edge_resize_extends_east_only() {
        let mut domain = LambertDomain::centered_at(35.5, -97.5, 100, 100, 3_000.0);
        let (west_lat, west_lon) = domain.latlon_at_grid(0.5, 50.5);
        // Drag the east edge out to grid x = 140.5 (40 more cells).
        let (lat, lon) = domain.latlon_at_grid(140.5, 50.5);
        resize_domain(&mut domain, Handle::Edge(1), lat, lon);
        assert_eq!(domain.nx, 140);
        assert_eq!(domain.ny, 100);
        // The west edge stayed put (within a small tolerance — the rebuild
        // re-centers the projection).
        let (west_lat_after, west_lon_after) = domain.latlon_at_grid(0.5, 50.5);
        assert!(
            (west_lat - west_lat_after).abs() < 0.05,
            "{west_lat} vs {west_lat_after}"
        );
        assert!(
            (west_lon - west_lon_after).abs() < 0.05,
            "{west_lon} vs {west_lon_after}"
        );
    }

    #[test]
    fn corner_resize_can_shrink_but_never_below_minimum() {
        let mut domain = LambertDomain::centered_at(35.5, -97.5, 100, 100, 3_000.0);
        let (lat, lon) = domain.latlon_at_grid(95.5, 95.5);
        resize_domain(&mut domain, Handle::Corner(0), lat, lon);
        assert_eq!(domain.nx, 16);
        assert_eq!(domain.ny, 16);
    }
}
