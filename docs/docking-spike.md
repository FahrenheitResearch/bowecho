# Docking / snappable-pane feasibility spike

Branch: `spike/docking` · Spike code: `crates/app_ui/examples/dock_spike.rs`
(`cargo run -p app_ui --example dock_spike`).

Original request summary: evaluate whether windows, panels, menus, soundings,
and viewers can be resized, snapped, docked, floated, and arranged into a
fully customizable workspace.

Verdict up front: **medium effort, low technical risk.** The hard parts
(drag-to-rearrange, splits, tabs, resize, layout persistence) are solved by
an off-the-shelf crate that is compatible with our exact egui version today.
The work is plumbing, not invention: converting existing `egui::Window`
content closures into pane draw functions and adding a dock/float toggle.

---

## 1. Crate evaluation

Two candidates, both with releases for egui 0.34 (we ship eframe **0.34.3**):

| | egui_tiles 0.15.0 | egui_dock 0.19.1 |
|---|---|---|
| Published | 2026-03-27 | 2026-03-31 |
| egui requirement | `egui ^0.34.0` (crates.io dependency metadata) | `egui ^0.34` |
| Maintainer | rerun-io org (same org/author as egui itself); drives the Rerun viewer in production | community project (originally lain-dono, now collaborative) |
| Model | tile tree: `Tabs`, `Linear` (h/v splits), `Grid` containers; auto-simplification | binary split tree of tab nodes; multiple "surfaces" |
| Floating windows | not built in (panes live in the tree) | **built in**: "Dragging tabs out into new egui windows" (README feature list) |
| serde persistence | `Tree<Pane>: Serialize/Deserialize` behind `serde` feature; JSON round-trip is covered by the crate's own `tests/serialize.rs` | `DockState` serde behind `serde` feature |
| Pane content API | `Behavior::pane_ui(&mut self, ui: &mut egui::Ui, TileId, &mut Pane) -> UiResponse` — plain `Ui` | `TabViewer::ui(&mut self, ui: &mut egui::Ui, &mut Tab)` — plain `Ui` |
| Pane-body drag | **opt-in**: body drags relocate a tile only if `pane_ui` returns `UiResponse::DragStarted` (and `Behavior::is_tile_draggable` agrees) | tabs drag from the tab bar; body is not drag-sensed |

No git pinning needed for either; both have proper crates.io releases for
egui 0.34. (egui_tiles 0.15.0 CHANGELOG: "Update to egui 0.34", MSRV 1.92.)

### Pick: `egui_tiles 0.15.0`

1. **Drag ergonomics is THE risk for a map app**, and egui_tiles makes it
   structurally impossible to get wrong: tab drags are sensed on the tab
   rect alone (`behavior.rs:122-127` — `ui.interact(tab_rect, id,
   Sense::click_and_drag())`), and a pane body can only start a tile drag
   by *explicitly returning* `UiResponse::DragStarted` from `pane_ui`
   (`tree.rs:406-410`). Our map pane returns `None`, so a body drag is
   always a map pan. Verified in the spike.
2. **Maintenance / version-skew**: released from the same org as egui, on
   egui's cadence. This workspace has already been burned by ecosystem
   version skew (fmt/clippy CI breakage on a sister branch); the crate most
   likely to update the day egui 0.35 ships is the one egui's own org
   maintains.
3. **Production pedigree for our exact shape**: Rerun renders wgpu
   paint-callback viewports inside egui_tiles panes. That is precisely our
   Volume Explorer (3D) pane and our textured map pane.
4. **`Grid` container** maps naturally onto a future evolution of the 1/2/4
   radar grid (v2+, see §3).
5. serde: tie. Both persist. egui_tiles' round-trip is exercised by its own
   test suite and re-proven in our spike (552-byte JSON, equality assert).

What we give up vs egui_dock: native **drag-out-to-floating-window**.
Mitigation: BowEcho already has floating `egui::Window`s and keeps them;
"snap to its own thing" becomes a *Float/Dock toggle button* (window title
bar ↔ tab context) in v1, which covers the requested capability (freely sizing
soundings, then snapping them into a window or panel) without the gesture. A
drag-out gesture can be added later (egui_tiles exposes
`Tree::dragged_id` + `move_tile`/remove APIs) or we re-evaluate egui_dock
if floating surfaces become the dominant workflow.

## 2. What the spike proved (compiled + ran, clippy-clean)

`crates/app_ui/examples/dock_spike.rs`, gated by
`cargo clippy --workspace --all-targets -- -D warnings` (green) and run
live (window event loop ran; stdout: `startup JSON round-trip OK (552
bytes)`).

1. **Custom-painter pane gets a correct rect + clip.** egui_tiles builds
   each pane `Ui` with `egui::Ui::new(..., UiBuilder::new()
   .layer_id(ui.layer_id()).max_rect(rect))` — "Each tile gets its own
   `Ui`, nested inside each other, with proper clip rectangles"
   (egui_tiles-0.15.0 `src/tree.rs:393-401`). So inside `pane_ui`,
   `ui.max_rect()` *is* the tile rect and `ui.clip_rect()` clips to it.
   The spike's map pane paints a full-rect gradient and prints both rects;
   the sounding pane intentionally overdraws past its right edge and gets
   clipped at the tile boundary.
2. **Map pan vs tile drag**: dragging the map pane body moves the spike's
   crosshair (pan state), never the tile. Tiles rearrange only from tabs
   or the explicit opt-in drag-handle button.
3. **Tabs + splits + drag-to-rearrange + resize** all work with zero code
   beyond `tree.ui(&mut behavior, ui)` (drop-preview included).
4. **Persistence**: `Tree<SpikePane>` ⇄ JSON string round-trips with
   equality, both at startup and via in-app Save/Restore buttons. Pane
   payloads (e.g. sounding name/hue — later: site, product) serialize *as
   part of the tree*, which is exactly what layout slots want.
5. **Floating window above the tree**: an `egui::Window` (the JSON viewer)
   floats over the tile tree without z-order issues — egui `Window`s live
   on `Area` layers above panel content, so deep-config dialogs can stay
   floating untouched.
6. **wgpu paint callback in a pane** (verified by API + Rerun precedent,
   not in the spike binary): `eframe::egui_wgpu::Callback::
   new_paint_callback(rect, ...)` takes an explicit rect; inside `pane_ui`
   that rect is `ui.max_rect()`/an allocated child rect, same as inside
   today's "Volume Explorer (3D)" window (`main.rs` on `feat/oa-catalog`,
   ~line 12731). Nothing about the callback API assumes a window.

## 3. Integration design

Reference tree for the mapping below: `feat/oa-catalog` (2026-06-11), which
carries the full current UI: `grid_canvas`/`pane_cell_rects` multi-pane
radar grid, `egui::Panel::right("product_tilt_panel")` sidebar, and the
floating windows ("Sounding (native)", "WoFS (Warn-on-Forecast)",
"Mobile Radar — FARM live", "Satellite (GOES)", "Volume Explorer (3D)",
"Model data", guide). The spike branch is based on
`fix/region-based-velocity-dealias`, which predates several of these;
integration PRs should land on the integrated UI line.

### 3.1 Panes vs floating windows

| Today | Becomes | Why |
|---|---|---|
| `grid_canvas` (CentralPanel; 1/2/4 cells via `pane_cell_rects(self.grid_layout, rect, 2.0)`) | **one pane** `WorkspacePane::RadarGrid` | The grid stays a single tile that internally manages its cells, **do not explode radar cells into tiles in v1.** The cells share one geo transform with a deliberate two-phase interact-then-paint loop ("no one-frame shear between panes during a drag") plus `active_pane` focus logic that the sidebar edits. Splitting cells into tiles would re-derive all of that through `Behavior` callbacks for zero user benefit — the cells already pan/zoom/resize as one synchronized surface. v2 *option*: an egui_tiles `Grid` container of radar panes with a shared-transform side channel, if per-cell free-form arrangement is ever wanted. |
| "Sounding (native)" window → `sounding_panels::draw_full(ui, rect, &sounding)` | **pane** `WorkspacePane::Sounding(id)` — the headline feature | Window body is already rect-based: it does `allocate_exact_size(available, hover)` then draws into the rect. As a pane: `let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover()); sounding_panels::draw_full(ui, rect, &sounding)` — unchanged. Multiple soundings = multiple panes in a `Tabs` container; "size it how I want" = drag the split; "snap to its own thing" = Float toggle. |
| "Satellite (GOES)", "WoFS", "FARM live" viewers | **panes**, each keeping a Float/Dock toggle (default: floating at first, so nothing changes until the user docks) | Content viewers, i.e. workspace material. All three draw textures/painters into rects — same pattern as the map. |
| "Volume Explorer (3D)" (wgpu paint callback) | **pane** | Callback takes an explicit rect; pane `Ui` supplies it. Its slider strip stays at the top of the pane. |
| "Model data" manager, "Model download (retired)", guide window, any settings dialogs | **stay floating `egui::Window`s** | Transient config dialogs, not workspace content. Floating windows render above the tree (proven in spike) — zero migration cost. |
| Sidebar `egui::Panel::right("product_tilt_panel")` (300–560 px, 4 tabs) | **stays an egui `Panel` outside the tile tree** (v1) | It is a control surface wired to `active_pane`/selection state, not content; egui panels already give resize; keeping it outside the tree means its width constraints, keyboard focus, and the overhaul's planned control regrouping ("+ Add layer" etc.) proceed independently of docking. Promoting it to a pane later is mechanical if ever wanted. |
| Top bar / status bar | stay `Panel::top`/`Panel::bottom` | Chrome, not content. |

### 3.2 Code shape in `ViewerApp`

- `ViewerApp` gains `workspace: egui_tiles::Tree<WorkspacePane>` where
  `WorkspacePane` is a small serde enum of IDs/config
  (`RadarGrid`, `Sounding(SoundingPaneId)`, `Satellite`, `Wofs`, `Farm`,
  `Vol3d`), **not** owning the heavy state — that stays on `ViewerApp`
  exactly where it is.
- Borrow split: `Behavior` needs `&mut ViewerApp` guts while the tree is
  also `&mut self`. Standard solution (Rerun does the same):
  `let mut tree = std::mem::take(&mut self.workspace); tree.ui(&mut
  WorkspaceBehavior { app: self }, ui); self.workspace = tree;`
  (`Tree: Default` when empty). `WorkspaceBehavior::pane_ui` then matches
  the pane enum and calls the same draw fns the windows call today.
- Each current `show_*` boolean becomes tri-state
  (`Hidden | Floating | Docked`). Dock = `tiles.insert_pane(...)` +
  insert into root container; Undock = remove tile (simplification prunes
  empty containers) + reopen the window. Tab close button → Floating or
  Hidden (pick one; recommend Floating so nothing is ever "lost").
- Single-pane cosmetics: with `SimplificationOptions::default()`
  (`prune_single_child_tabs: true`, `all_panes_must_have_tabs: false`) a
  lone RadarGrid pane renders with **no tab bar** — day-one screenshots are
  pixel-identical to today. Tab bar height is 24 px (`Behavior::
  tab_bar_height`), overridable.

### 3.3 Layout persistence

- `Tree<WorkspacePane>` serializes to a compact JSON value (spike: 552
  bytes for 4 panes). Store it in `AppSettings`
  (`crates/settings/src/lib.rs`, `%APPDATA%\bowecho\config.json`) as
  `workspace_layout: Option<serde_json::Value>` wrapped in
  `{ "version": 1, "tree": ... }` for forward migration. Load is
  best-effort like everything else in `AppSettings` (parse failure →
  default layout), matching the existing `from_json` philosophy.
- **Layout slots (no setting exists yet — the old count-only
  `saved_layout_slots` field was removed as dead)**: implement slots as
  `Vec<SavedLayout { name: String, layout: serde_json::Value }>`; a slot
  is just a named tree snapshot. Because pane payloads serialize inside
  the tree, a slot intrinsically captures *which* soundings/viewers were
  docked where, plus `grid_pane_count` stays the radar pane's own field as
  today. This supersedes ad-hoc per-window pos/size persistence.
- Customization profiles (inspector-card config on `feat/ui-refresh`)
  stay orthogonal: they configure pane *content*, layouts configure pane
  *geometry*. A profile switch must not rewrite the tree.

### 3.4 Risks (ranked, with what the spike showed)

1. **Input routing — tile drag vs map pan.** The classic failure. Solved
   by crate design: pane-body drags relocate tiles only via explicit
   `UiResponse::DragStarted` (`tree.rs:406`); tab drags sense only the tab
   rect (`behavior.rs:122`). Residual: the radar pane keeps its own
   per-cell `ui.interact` loop — works identically inside a pane `Ui`
   because interaction ids hang off `ui.id()`. Watch one thing: ids. The
   pane `Ui` id is `tree_ui.id().with(tile_id)`; any code that built ids
   from absolute sources is unaffected, but code using `ui.id()` gets new
   ids after a re-dock (egui state like collapsing-headers may reset —
   cosmetic).
2. **Z-order / floating windows.** `egui::Window`s float above the tree
   (spike-verified). Two residuals: (a) egui_tiles' drag preview is an
   `egui::Area` (`tree.rs:455`) — while dragging a tab it can render under
   an open floating window; cosmetic, accept. (b) Anything painted with
   `painter_at`/overlay tricks inside a pane must use the pane rect, not
   screen rect — `grid_canvas` already takes an explicit `rect` so it
   composes.
3. **Clip rects for painter-heavy panes.** Pane `Ui` has correct
   clip (`tree.rs:393`); spike's overdraw test clipped correctly. Residual:
   our draw fns that compute layout from `ui.available_size()` *before*
   allocating must do so inside the pane (they do today inside windows —
   same pattern).
4. **Borrow-splitting in a 21k-line `main.rs`.** Mechanical but touchy;
   mitigated by the `mem::take` pattern and by doing the window→pane moves
   one window per PR.
5. **wgpu callback pane.** Lowest risk: rect-parameterized API +
   Rerun precedent; the existing window code moves verbatim. Verify
   scissor behavior at fractional DPI scale once (same check it needed as
   a window).

### 3.5 Effort: PR-sized steps and sequencing vs the UI overhaul

Overall: **medium** — about 5 small/medium PRs of plumbing; no research
risk left. Suggested sequence (each independently shippable, gates green):

1. **PR1 — skeleton (S):** dev-dep → real dep; `WorkspacePane` +
   `WorkspaceBehavior`; tree with the single `RadarGrid` pane replacing
   the direct `map_canvas/grid_canvas` call in CentralPanel. Simplification
   hides the tab bar → pixel-identical app. Pure refactor, easy review.
2. **PR2 — soundings dock (S/M):** "Sounding (native)" gains Dock/Float;
   `sounding_panels::draw_full` as pane; multiple sounding panes in a
   `Tabs` container. *This alone delivers the owner's headline ask.*
3. **PR3 — viewers dock (M):** Sat/WoFS/FARM/Vol3d tri-state
   (Hidden/Floating/Docked), default Floating. One window per commit.
4. **PR4 — persistence (S):** `workspace_layout` in `AppSettings`
   (versioned), restore on startup, reset-to-default action.
5. **PR5 — layout slots (S/M):** named slots UI backed by a new
   `Vec<SavedLayout>` setting; save/load/rename.
6. **v2 (optional):** drag-out-to-float gesture; radar cells as a `Grid`
   container; sidebar as a pane.

**Sequencing: docking is the overhaul's step-1 skeleton, not its finale.**
Arguments: (a) the tile tree is the lowest layer of the UI — every overhaul
decision about where content lives renders *inside* some container, and
landing containers first gives the overhaul stable mount points ("a pane is
the unit of workspace UI") instead of restyling windows that are about to
be re-parented; (b) PR1 is a behavior-preserving refactor, cheapest to
merge before `feat/ui-refresh`'s ongoing `main.rs` churn compounds the
conflict surface — both edit the same regions (window fns, CentralPanel),
and rebasing 5 small docking PRs over a finished overhaul is strictly more
work than building the overhaul on the skeleton; (c) the parts of the
overhaul that don't touch the tree (sidebar regrouping, layer rows, top
bar) proceed in parallel because the sidebar/top bar deliberately stay
outside the tree. The counter-case (docking last) only wins if the overhaul
plans to delete or merge the floating windows themselves — it doesn't; it
reorganizes controls.

---

*Spike evidence trail: crates.io dependency metadata for egui_tiles 0.15.0
/ egui_dock 0.19.1; egui_tiles 0.15.0 sources in the cargo registry
(`tree.rs`, `behavior.rs`, `tiles.rs`, `examples/simple.rs`,
`tests/serialize.rs`); rerun-io/egui_tiles CHANGELOG; Adanos020/egui_dock
README; `feat/oa-catalog` `crates/app_ui/src/main.rs` for the current
window/grid inventory.*
