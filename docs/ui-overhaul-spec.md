# BowEcho UI Overhaul Spec v2 — "Finish the Layer Rail"

**Status:** supersedes `docs/ui-refresh-proposal.md` (v0.8.2 audit) and `docs/sidebar-redesign-spec.md` (v1 sidebar). Written against branch `fix/region-based-velocity-dealias` @ `7ba98aa` (v0.14.1, `crates/app_ui/src/main.rs` = 22,698 lines, eframe 0.34.3). Line numbers will drift; every reference names the function, which is durable.

**Read this first — what the audit actually found:**

1. `feat/ui-refresh` is **not stale — it is fully merged** (an ancestor of current HEAD). Old-proposal steps 2 (Model-button intent fix), 3 (`layer_row()` grammar + honest count), 4 (settings evicted from the fold), 5 (`+ Add layer ▾`), and 8 (Download merged into the Model window) all shipped. Do not re-plan them; build on them.
2. Old-proposal steps that **never landed**: 1 (top-bar regroup + Sounding front door), 6 (LAYERS promoted to a tab), 7 (map-anchored warning card), 9 (viewport tear-off), 10 (session layout). These carry forward, re-scoped below.
3. The proposal's own prophecy — "the next spec must define where new layer types and new data windows GO, or v1.1 will accrete the same way" — **came true**. Since v0.8.2 the app grew: WoFS, FARM, 3D, Guide (4 more top-bar buttons → 8 controls + a width-morphing live chip), and inside the Layers fold: a Surface-obs row with 3 inline sub-toggles, a bare-checkbox GLM row, an SPC row with a combo + 5 checkboxes, a 150-line OA/mesoanalysis workbench (Analyze obs / RAOB / Compute composites / two `▾` menus), and a Poll-URL acquisition row with a Feeds menu. The fold body inside `radar_controls_panel` is now ~750 lines and once again mixes three different kinds of thing: **layers** (rows), **compute** (OA workbench), and **acquisition** (Poll URL).
4. `AppSettings::favorites` is written (`remember_startup_site`, main.rs:5419) but **never read** — there is no favorites UI despite the data existing.
5. The Sounding window still has no front door: it opens only as a side effect of `poll_native_sounding` and once closed cannot be reopened without re-Alt-clicking.

**The verdict:** Direction A ("everything is a layer") was right and is half-built. The crowding is not density — GR2A users tolerate density — it is that one tab (RADAR) hosts four jobs and the top bar hosts seven windows. The fix is to **finish the rail, split the jobs into tabs, and give windows one scalable home**. No new dependencies, no docking, no first-run tour.

---

## 1. INFORMATION ARCHITECTURE — five tabs

`SidebarTab` (main.rs:2476) grows from 4 to 5 variants; Settings becomes a gear icon to keep ≥60 px per text tab at the 300 px minimum width:

```
RADAR · LAYERS · SEVERE · DATA · ⚙
```

Every tab body is a `ScrollArea` with a stable `id_salt` (existing pattern in `side_panel`, main.rs:6420). Sections inside tabs are `CollapsingHeader`s with **fixed `id_salt`s and open-state mirrored into `AppSettings`** — eframe is built without the `persistence` feature (workspace `Cargo.toml:15`), so egui Memory does not survive restarts; add `sidebar_section_open: BTreeMap<String, bool>` to `AppSettings` and write it on change (same pattern as `save_overlay_defaults`).

### RADAR — operate the primary radar (volume-centric, mostly unchanged)

| Control | Current location | Disposition |
|---|---|---|
| Panes 1/2/4 + editing-pane notice | `radar_controls_panel` top (6677–6716) | keep, row 1 |
| SITE: site combo + Center | 6720–6744 | keep; **add favorites chip row** under the combo: small selectable chips from `app_settings.favorites` (finally reading the dormant field), each chip = one-click site switch + load-latest |
| Load Latest / Load Loop / Live / Chunks / Open… | 6745–6784 | keep |
| One-line status + live-chunk readout + ▸ Volume details | 6786–6825 | keep |
| **Layers fold (6827–7573)** | inline in `radar_controls_panel` | **REMOVE from this tab** — body becomes the LAYERS tab (§2). RADAR keeps a one-line link-row: `Layers: 7 (2 hidden) →` that switches to the LAYERS tab |
| PRODUCTS grid + VEL/SRV/hail contextual rows + Color + Hide-below + Gate filter | 7608–7870 | keep verbatim |
| TILT header + list | 7872–7936 | keep |
| LOOP (`frame_history_panel`) | 7938–7940 | keep |
| ALGORITHMS: Rotation markers, Storm tracks + SRV←tracks | 7942–7984 | keep here, **not** in the rail — they are volume-gated radar algorithms parameterized by radar state (storm motion), and their toggles belong next to the products they annotate. (They get rail rows only if they ever grow opacity/order needs.) |
| TOOLS: Inspector… menu, Inspector card, Vrot, Cross-section | 7986–8037 | keep; **the incoming RHI window's "arm RHI azimuth pick" control lands here** as a third armed tool, same checkbox + Clear pattern as Vrot/XS |

Net effect: the Radar tab loses its largest scroll cost (the fold) and becomes what its tooltip claims: "site, products, tilt, loop, algorithms — live operations".

### LAYERS — the rail (new tab; §2 is the full spec)

Everything drawn over the map, one row grammar, plus the `+ Add layer ▾` front door and the OA analysis section (which *produces* layers).

### SEVERE — warnings + SPC in one place (rename of Warnings)

| Control | Current location | Disposition |
|---|---|---|
| Show / Active / Auto checkboxes | `hazard_panel` 8455–8459 | keep, rename "Active only" / "Auto-refresh" |
| Family filter wrap (TOR/SVR/FFW/…) | 8460–8478 | keep |
| Fill slider | 8479–8486 | keep (also exposed as the warnings rail-row opacity — same state, two views) |
| Refresh Live / Clear | 8487–8498 | keep |
| Selected-hazard detail scroll | 8500–8513 | keep, **plus** the map-anchored warning card (old step 7, §6 PR-6): polygon click pops an `egui::Area` card at the click (event · expiry · hail/wind tags · "Full text →" link to this tab), reusing `hazard_record_detail_lines` (16217) |
| Summary scroll + ▸ Local file | 8515–8540 | keep |
| **SPC config** (day combo + cat/torn/wind/hail + Reports) | currently jammed into one fold row (6019–7060) | moves here as section "SPC OUTLOOKS": day picker + kind checkboxes + Reports toggle. The rail shows only the two SPC rows (§2); their ⚙ jumps here |

### DATA — acquisition and sources (Archive absorbs its siblings)

| Control | Current location | Disposition |
|---|---|---|
| Loop transport duplicate | `archive_panel` 5568 | keep (deliberate duplication, still correct) |
| Frames fetch-count + "+5 earlier" | 5570–5593 | keep; rename label "Frames" → "Fetch N scans" (kills the two-frames-numbers confusion, old issue 1.3.9) |
| Date nav + volume list + On-click Loop/Single | 5594–5682 | keep |
| Tornadoes (SPC) fetch + report list | 5683–5739 | keep (it is archive-date-scoped event data, not a live severe layer) |
| **Poll URL + Feeds ▾ + Start/Stop** | Layers fold 7323–7377 | **moves here** as section "LIVE FEEDS" — it is acquisition (it *replaces the primary volume source*), not a layer; the fold never should have held it |
| **Model store status + Fetch latest / Download… link** | Model window Download fold (10398–10490) | window keeps the full panel; DATA gets a two-line "MODEL STORE" section: newest-run readout + one `Download…` button setting `model_dock_open + model_download_open` (the existing one-shot expand path) |
| Local radar file Open… | duplicated from RADAR ▸ SITE | second entry point here, same `start_local_volume_load` |

### ⚙ (Settings) — unchanged content, icon tab

`settings_panel` (6550) keeps Display / Color tables / Hotkeys / Performance / Model sections verbatim. Tab label becomes the gear glyph with tooltip "Settings". The Hotkeys section is rewritten in §5's registry step to list *everything*, not just the number row.

---

## 2. THE LAYER RAIL

### 2.1 Row grammar v2

`layer_row` / `LayerRowSpec` (main.rs:16140 / 16121) already implements:

```
[vis] [name (hover=details)] [state-dot] [opacity ────] [trailing…]
```

Extend the spec struct — do not fork it — with two standardized slots:

```rust
struct LayerRowSpec<'a> {
    vis: LayerRowVis<'a>,          // Toggle | Badge (exists)
    name: &'a str,                 // exists
    name_width: f32,               // exists — tiers: 42 (site IDs) / 96 (standard) / 150 (placefiles)
    name_hover: &'a str,           // exists
    state: Option<&'a str>,        // exists (dot + hover)
    opacity: Option<LayerRowOpacity<'a>>, // exists (F32 | U8)
    order: Option<LayerRowOrder<'a>>,     // NEW: ↑/↓ — uniform reorder slot
    gear: Option<LayerRowGear<'a>>,       // NEW: ⚙ — see contract below
}
```

- **Order**: standardized ↑/↓ small-buttons (the model rows' existing pattern, 7104–7119, generalized). **Deliberately not drag-and-drop in v1**: the priority persona operates a trackpad in a moving truck; two 18 px buttons beat a drag gesture under stress, and egui's built-in dnd can be layered on later without changing the spec. Reorder applies *within a rail group* (draw order is group-major, see 2.2).
- **Gear contract (the extensibility rule):** `⚙` opens the layer's **owning surface** — a window (`model_dock_open`, `show_satellite`, `wofs.open`, `farm.open`) or a tab section (SEVERE ▸ SPC) — or, for layers with only 2–3 small options, an inline popover (`ui.menu_button` with the gear glyph). **A row may carry at most two inline extras besides ⚙/✕**; everything else goes behind the gear. This single rule is what keeps the next five features from re-crowding the rail.
- `✕` stays a trailing extra where removal makes sense; the primary radar keeps the `◉` badge and no ✕.

### 2.2 Rail structure — grouped, fixed order

Groups are weak uppercase mini-headers (NOT collapsing — the rail is one scannable list; collapsing returns the junk-drawer dynamics). Draw order on the map = bottom-to-top within the list, group-major:

```
BASE        primary radar, overlay radars
ATMOSPHERE  model fields, OA/composite fields, GOES, WoFS drape, FARM drape
OBS         surface obs, lightning (GLM)
SEVERE      SPC outlook, SPC reports, warnings
COMMUNITY   placefiles, (future: LSRs, spotter feeds)
```

### 2.3 Complete mapping — every existing toggle onto the rail

| Layer | Today (location) | Rail row spec |
|---|---|---|
| Primary radar | `layer_row` w/ Badge, fold 6849–6884 | unchanged; stays row 1 of BASE |
| Overlay radars | `radar_layers_panel` (8199) rows w/ Go/Ref/Pri/x | keep `layer_row`; inline extras = **Go** + **✕** (the storm-hop workflow earns inline); **Ref + Pri move behind ⚙ popover**; "Overlays N + Clear" header line stays as the BASE group's right-aligned action |
| Model field layers | fold 7078–7172, ↑/↓/⚙/✕ | unchanged semantics; ↑/↓ migrate to the `order` slot; the **Hour ◀ ▶ stepper** moves from below the rows to the ATMOSPHERE group header (it steps all dock-following rows) |
| OA / composite fields | pushed into `model_layers` via `push_composite_layer` (7289–7291) | same rows as model fields, name suffix "(OA)" (already the behavior) — no special-casing |
| GOES | fold 6889–6939 (`layer_row` + ⚙ + ✕) | unchanged; ⚙ → Satellite window (already correct) |
| **WoFS drape** (incoming branch) | — | NEW row in ATMOSPHERE: vis toggle, name `WoFS <product>`, state = `<init>z+<min>` (hover: run/init/minute/sync), opacity F32, ⚙ → WoFS window, ✕ removes drape. The branch's "map-drape toggle" checkbox **must land as this row**, not as a window checkbox — the window keeps a "Show on radar map" button (the Sat/Model convention, 10800–10813) that *creates* the row |
| **FARM drape** (incoming branch) | — | NEW row in ATMOSPHERE: vis, name `FARM <sensor>`, state = live dot (`is_live()`), opacity, ⚙ → FARM window, ✕. Same "Show on radar map creates the row" convention |
| Surface obs | fold 6940–6999 (`layer_row`, sub-toggles inline in trailing) | keep row; **METAR / Mesonet / adj-snd checkboxes move behind ⚙ popover** (they violate the 2-extra budget today); state slot = `N stn · Xm` (already computed) |
| Lightning (GLM) | bare checkbox row 7000–7018 | **promote to `layer_row`**: vis = `glm_enabled`, name "Lightning", state = `N fl/10m`, no opacity v1 (age-fade is intrinsic), ⚙ popover = satellite source pick (goes19/goes18, future) |
| SPC outlook | combo + 4 checkboxes + label, 7019–7048 | **one row**: vis = any kind enabled (toggling off disables all kinds, on restores last set), name `SPC D{n} outlook`, state = fetch spinner/age, no opacity (SPC's own colors), ⚙ → SEVERE ▸ SPC OUTLOOKS section |
| SPC reports | checkbox in same row 7049 | own row: vis = `spc_reports_enabled`, name "SPC reports", ⚙ → SEVERE tab |
| Warnings | not in fold at all (tab-only) | NEW row: vis = `hazards_visible`, name "Warnings", state = active count, opacity = `hazard_fill_alpha` (U8, 0–80 range), ⚙ → SEVERE tab. Fixes "the warnings layer is invisible in the layer model" |
| Placefiles | fold 7378–7465 (`layer_row` + T/↻/✕) | unchanged — T + ↻ are exactly the 2-extra budget; URL input + Add stays as the COMMUNITY group footer |
| Poll URL | fold 7323–7377 | **NOT a layer** → DATA tab (§1) |

### 2.4 `+ Add layer ▾` (fold 7470–7572) — keep as the rail footer, grow it

Existing entries (Radar overlay ▸ sites, Model field…, SpotterNetwork, Get model data…, Satellite…, Surface obs, Placefile URL…) carry over. Add:

- `WoFS drape…` → opens WoFS window (row born from its "Show on radar map")
- `FARM drape…` → opens FARM window (same)
- `Mesoanalysis (OA) ▸` → **this is the extensible home for the incoming composites-catalog branch**: a submenu organized like SPC's mesoanalysis page (Thermodynamics / Kinematics / Composite indices / …), where each leaf either adds the cached field instantly (post-compute) or triggers the compute. The current flat `Composites ▾` menu (7259–7280) migrates into this tree.
- The site-picker submenu gets a **favorites-first** ordering (read `app_settings.favorites`).

### 2.5 ANALYSIS (OA) — compute lives at the rail's bottom, not among rows

The OA workbench (fold 7173–7319: Analyze obs, RAOB sounding, Compute composites + progress, Derive (OA) ▾) is *compute that emits layers*. It moves to a collapsing section **at the bottom of the LAYERS tab** named "ANALYSIS (OA)", default-closed, gated as today (`dock_has_field`). Rationale against burying it in the Model window: the analyst persona runs Analyze-obs during ops with the sidebar open; rationale against leaving it inline among rows: it is the single biggest reason the fold reads as a junk drawer. Its disabled-state hint strings ("← turn on Surface obs above" etc., 7210–7218) are kept verbatim — they are the best self-explaining UI in the app.

---

## 3. TOP BAR (`top_bar`, main.rs:6282)

Today: `BowEcho | Reset View | Reload | Sat | Model | WoFS | FARM(→"<name> LIVE")| 3D | Guide | [update chip]` — eight controls with three different semantics, and the FARM button *changes width* when a sensor goes live (layout shift mid-ops).

Target:

```
BowEcho │ Reset View · Reload │··············│ [DOW8 LIVE] [v0.15 ↑] │ Windows ▾ · Guide
  brand    one-shot actions       (spacer)        status chips         menus (right)
```

- **Stays:** Reset View, Reload (one-shot actions, left). Guide (top-level, right — it is the discoverability anchor and must never be buried; the owner's no-tours rule makes Guide + hover text the *only* teaching surface).
- **Collapses into `Windows ▾`** (a `menu_button`): Model data, Satellite, WoFS, FARM, 3D Volume, **Sounding** (new — `native_skewt_open` toggle, enabled iff `native_sounding.is_some()`; finally a front door), and the **incoming RHI window** lands here as entry #7 with zero top-bar churn. Each entry renders as a checked/unchecked toggle with its hotkey hint (§5). The Model entry keeps the intent rule (open ⇒ `model_enabled = true`, 6308–6314).
- **Chips, far right, fixed-width-reserved:** the FARM LIVE chip (green, click = open FARM window + `select_sensor(live_id)` — current behavior at 6325–6347, divorced from the window-toggle button) and the update-available chip (6364–6382, unchanged). Chips are *status*, buttons are *commands*; they no longer share widgets.
- Rationale for a menu over an icon strip: window count is 7 going on 9 (RHI, future obs-sounding browser); a strip re-crowds within two releases, and every window also remains reachable from its rail-row ⚙ and its hotkey — the menu is the third path, not the only one. GR2A precedent: deep dialogs live in menus there too.

---

## 4. DENSITY RULES

Extract a `mod ui_theme` (new file `crates/app_ui/src/ui_theme.rs`) holding the existing magic numbers (main.rs:134–143) plus the new contract, and have `configure_style` (2501) read from it:

| Constant | Value | Use |
|---|---|---|
| `ROW_H` (= `PANEL_BUTTON_HEIGHT`) | 24.0 | every button/row/slider height — no exceptions |
| `ROW_SPACING_X` | 3.0 | inside layer rows + tab bar (already the convention) |
| `SECTION_SPACING` | 8.0 + separator | `section_header` (6466) — keep |
| `SUBHEAD_COLOR` | rgb(148,160,172) | section + rail-group headers (matches guide.rs:12) |
| `ACCENT_COLOR` | rgb(120,168,220) | editing-pane notice, keycap hints (guide.rs:14) |
| `LIVE_COLOR` | rgb(110,245,130) | live chips/dots only — never decorative |
| `COMBO_MAX_W` | 220.0 | no combo wider (site combo fills, capped) |
| `NAME_W_SITE / STD / WIDE` | 42 / 96 / 150 | the three `name_width` tiers — pick one, never a fourth |
| Sidebar | 300–560, default 380 | unchanged (`SIDEBAR_*_WIDTH`) |

**Icons vs labels:** glyph-only is allowed exactly for the universal set already in use — `↑ ↓ ✕ ⚙ ↻ ◀ ▶ ⏸ ◉` — and each MUST carry `on_hover_text`. Everything else is a text label. Buttons that exist in both a window and the rail use identical strings ("Show on radar map") so the Guide can name them once.

**Discoverability doctrine (no tours, ever):** every interactive control has hover text; hover text names its hotkey where one exists ("hotkey 3" pattern, 7626–7628); the Guide (guide.rs) gains a "Layers" section in PR-4 and its Shortcuts section is generated from the keybinding registry in PR-7 so docs cannot drift from bindings. One-line status + hover-for-detail stays the law (R1 status line is the model).

---

## 5. KEYBOARD MAP — conflicts check and additions

**Existing bindings (preserve byte-for-byte; all routed through `text_edit_focused()` + `consume_key`, `handle_keyboard_navigation` 3759):**

| Binding | Action | Anchor |
|---|---|---|
| `1–9, 0` | products (remappable; REF VEL SRV RHO ZDR SW CREF ET VIL VILD) | `handle_product_hotkeys` 3811; defaults settings/src/lib.rs:89 |
| `←/→` | step product (focused pane) | 3769–3787 |
| `↑/↓` | step tilt (focused pane) | 3789–3806 |
| Shift+click | pin/release inspector | 9145 |
| Alt+click / Ctrl+Alt hover | model sounding / follow-mouse | 9222–9227 |
| Ctrl+click (no Alt/Shift) | switch to lowest-beam radar | 9272–9283 |
| Right-click | best-radar menu / clear armed tool | 9101–9106, 9269 |

**Conflict findings:**
1. Number row is consumed only when a volume exists (`handle_product_hotkeys` early-returns, 3824) — new no-volume bindings on digits would be ambiguous; **do not bind digits to anything else**.
2. `Ctrl+1..4` (pane focus, proposed) does not collide — product keys consume `Modifiers::NONE` only. Safe.
3. `Space` collides with egui's button activation when a widget has keyboard focus; gate on `ctx.memory(|m| m.focused().is_none())` in addition to `text_edit_focused`.
4. Arrow keys vs egui slider focus: already handled by the consume-key-at-frame-start order (keyboard handler runs before panels, 6225). Keep that ordering for all new keys.
5. The GR2A muscle-memory conflict (`←/→` = frames there) stands: ship the `key_profile: "bowecho" | "gr2a"` setting (old §5.2), surfaced in Guide ▸ Shortcuts, not as a tour.

**New bindings (PR-7, all remappable):** `Space` play/pause loop · `,`/`.` frame step · `Esc` disarm tool → close warning card → close topmost window · `X` cross-section · `R` Vrot · `I` inspector card · `L` LAYERS tab toggle · `W` SEVERE tab toggle · `Ctrl+1..4` pane focus · `F1` Guide · `Ctrl+M/S/F/O/D/3` window toggles (Model/Sat/FARM/WoFS soundings/3D — exact letters decided in PR-7, shown in the Windows ▾ menu). Implementation: generalize to `key_bindings: BTreeMap<String,String>` (action-id → key) in `AppSettings`, leaving `product_hotkeys` untouched for back-compat; Settings ▸ Hotkeys and Guide ▸ Shortcuts both render the registry.

---

## 6. MIGRATION PLAN — eight PR-sized steps

Ordering principle: **scaffold the homes before the five feature branches land**, so they merge into the new structure instead of bolting onto the fold and being migrated twice. Every step compiles and ships alone. Coordination gate: confirm no concurrent `app_ui` work, branch from the current mainline, and never touch dealias/render-worker code.

| PR | Scope | Exact moves | Feature-branch gates |
|---|---|---|---|
| **1** | **Top bar: Windows ▾ + chips + Sounding front door.** Rebuild `top_bar` (6282): left actions; `Windows ▾` menu (Sat/Model/WoFS/FARM/3D/Sounding toggles, Model keeps intent rule); FARM LIVE + update chips right-aligned fixed slots; Guide stays top-level. | edit `top_bar`; new helper `fn windows_menu(&mut self, ui)` | **Land before everything.** RHI branch then adds one menu entry — trivial merge |
| **2** | **Row grammar v2.** Add `order`/`gear` slots to `LayerRowSpec` (16121) + `layer_row` (16140); port GLM (7000) and SPC (7019) onto `layer_row`; add the Warnings row; move obs sub-toggles + overlay Ref/Pri behind ⚙ popovers; model-row ↑/↓ → `order` slot. All **in place inside the existing fold** — zero relocation. | `layer_row`, `radar_layers_panel`, fold body | drape branches SHOULD wait for this (their rows use v2) |
| **3** | **Extraction, no movement.** Split `radar_controls_panel` (6661): `fn layers_rail(&mut self, ui, ctx)` (fold body minus the two evictions), `fn live_feeds_section` (Poll URL block 7323–7377), `fn oa_analysis_section` (7173–7319), `fn add_layer_menu` (7470–7572). Call sites unchanged — the fold still renders in RADAR. Pure churn-minimizer. | new fns, same output | **Merge point A: WoFS-drape + FARM-drape branches land here** — each adds one row to `layers_rail` + a "Show on radar map" button in its window, per §2.3 row specs. WoFS sounding-station-picker branch is window-internal: lands any time |
| **4** | **Tab promotion.** `SidebarTab` (2476) → `{Radar, Layers, Severe, Data, Settings}`; `SIDEBAR_TABS` (2483) + tooltips (2490); `side_panel` match (6425) routes LAYERS → `layers_rail` + `add_layer_menu` + `oa_analysis_section`, DATA → `archive_panel` + `live_feeds_section` + model-store section; RADAR gets the "Layers: N →" link-row; Settings tab label → ⚙ glyph. Section open-states mirrored into `AppSettings.sidebar_section_open`. Guide gains a "Layers" section. | `SidebarTab`, `side_panel`, `archive_panel`, guide.rs | do not start until A is merged (conflict surface = the fold) |
| **5** | **Favorites + rail polish.** Favorites chip row in RADAR ▸ SITE + favorites-first site lists (read `app_settings.favorites`; add remove-affordance on right-click); rail group headers + group-major draw order; Hour stepper to ATMOSPHERE header. | `radar_controls_panel`, `layers_rail`, layer compositor draw order | — |
| **6** | **SEVERE consolidation + warning card.** SPC config section into SEVERE; rail SPC/Warnings ⚙ jump there; map-anchored polygon card (`egui::Area` at click, `hazard_record_detail_lines`), behind a Settings checkbox if nervous. | `hazard_panel`, new `fn warning_card_overlay` | — |
| **7** | **Keyboard registry.** `key_bindings` map in `AppSettings`; new bindings per §5; Settings ▸ Hotkeys renders full registry; Guide ▸ Shortcuts generated from it; Windows ▾ entries show their keys. Keyboard-neutral until this PR — steps 1–6 add no keys. | `handle_keyboard_navigation`, settings lib, `hotkeys_section`, guide.rs | **Merge point B: composites-catalog branch** lands here or after — its menu tree slots into `add_layer_menu ▸ Mesoanalysis (OA)` (§2.4) and `oa_analysis_section` |
| **8** | **Session layout + viewport tear-off (stretch).** Persist open windows / tab / pane grid / layer set in config.json; then `ctx.show_viewport_deferred` Detach for Sounding → Sat → Model (old step 9; the only platform-QA step — texture sharing + repaint wakeups on Windows/macOS). Cut this PR before cutting anything above it. | `AppSettings`, window fns | post-v1 candidate |

---

## 7. WIREFRAMES

### 7.1 Default storm view (1× pane, volume loaded)

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ BowEcho │ Reset View · Reload          [DOW8 LIVE] [v0.15↑]  Windows ▾ · Guide│
├─────────────────────────────────────────────────────┬────────────────────────┤
│                                                     │ RADAR LAYERS SEVERE    │
│                                                     │       DATA  ⚙          │
│                 MAP CANVAS                          ├────────────────────────┤
│        (colorbar · mode chip · inspector)           │ Panes  1 2 4           │
│                                                     │ ── SITE ──────────────│
│                                                     │ [KTLX — Oklahoma… ▾][Center]
│                                                     │ ★KTLX ★KEAX ★KFDR     │ ← favorites chips
│                                                     │ [Load Latest][Load Loop]│
│                                                     │ ☐Live ☐Chunks [Open…] │
│                                                     │ KTLX · VCP 212 · 22:41Z · 14 cuts
│                                                     │ Layers: 7 (2 hidden) → │ ← link to LAYERS
│                                                     │ ── PRODUCTS ──────────│
│                                                     │ 1·REF 2·VEL 3·SRV 4·RHO│
│                                                     │ 5·ZDR 6·SW 7·CREF …   │
│                                                     │ ☑Unfold [Region ▾] ☐Flip│
│                                                     │ Color [NWS Velocity ▾] Edit…
│                                                     │ ☐Hide |val| below      │
│                                                     │ ☐Gate filter           │
│                                                     │ ── TILT ──────────────│
│                                                     │ ↑/↓   #00 0.48° 720 …  │
│                                                     │ ── LOOP ──────────────│
│                                                     │ [<][Pause][>] 9/10 [10▾]
│                                                     │ ▬▬▬▬▬▬▬●▬              │
├─────────────────────────────────────────────────────┤ ── ALGORITHMS ────────│
│ Rendering KTLX… │      9 frames · 1 overlay · 230 km│ ☑Rotation ☑Tracks      │
└─────────────────────────────────────────────────────┴────────────────────────┘
```

### 7.2 LAYERS tab (rail expanded)

```
│ RADAR │LAYERS│ SEVERE │ DATA │ ⚙ │
├───────────────────────────────────┤
│ BASE                    Clear     │
│ ◉ KTLX REF 0.5°   live● ▓▓▓▓▓░    │  ← primary: badge, no ✕
│ ☑ KEAX            live● ▓▓▓░░ [Go][⚙][✕]      ⚙: Refresh · Make primary
│ ATMOSPHERE              Hour ◀ ▶  │  ← stepper on group header
│ ☑ REFC f02        12z●  ▓▓▓▓░ [↑][↓][⚙][✕]    ⚙ → Model window
│ ☑ SBCAPE (OA) f02  OA●  ▓▓░░░ [↑][↓][⚙][✕]
│ ☑ GOES-19 C13           ▓▓░░░ [⚙][✕]          ⚙ → Sat window
│ ☑ WoFS UH-paint  21z+45 ▓▓▓░░ [⚙][✕]          ⚙ → WoFS window   (incoming)
│ ☑ FARM DOW8      live●  ▓▓▓▓░ [⚙][✕]          ⚙ → FARM window   (incoming)
│ OBS                               │
│ ☑ Surface obs  312stn·3m      [⚙] │  ⚙: ☑METAR ☑Mesonet ☐adj snd
│ ☑ Lightning    47 fl/10m      [⚙] │
│ SEVERE                            │
│ ☑ SPC D1 outlook  ✓●          [⚙] │  ⚙ → SEVERE tab
│ ☑ SPC reports                 [⚙] │
│ ☑ Warnings  12 act  fill▓░    [⚙] │
│ COMMUNITY                         │
│ ☑ SpotterNetwork    [T][↻][✕]     │
│ [https://… placefile URL  ][Add]  │
│                                   │
│ [+ Add layer ▾]                   │  Radar overlay ▸ · Model field… ·
│                                   │  WoFS drape… · FARM drape… ·
│                                   │  Mesoanalysis (OA) ▸ ▸ Composite indices ▸ STP(eff)…
│ ▸ ANALYSIS (OA)                   │  Satellite… · Surface obs · Placefile URL…
│   [Analyze obs] [Obs sounding]    │
│   [Compute composites] 412/1024 ⟳ │
└───────────────────────────────────┘
```

### 7.3 SEVERE tab

```
│ RADAR │ LAYERS │SEVERE│ DATA │ ⚙ │
├───────────────────────────────────┤
│ ☑Show ☑Active only ☐Auto-refresh  │
│ ☑TOR ☑SVR ☑FFW ☑Flood ☐SMW ☐SQW   │
│ ☑Watch ☑MD ☐SPS                   │
│ Fill ▓▓░░░░░░ 28                  │
│ [Refresh Live] [Clear]            │
│ ┌ selected ────────────────────┐  │
│ │ Tornado Warning  KTLX 22:58Z │  │  ← also pops as map card
│ │ …radar-indicated, 70mph hail…│  │     at the polygon (PR-6)
│ └──────────────────────────────┘  │
│ 84 scanned · 12 polygons · live   │
│ ── SPC OUTLOOKS ─────────────────│
│ Day [D1 ▾] ☑Categorical ☑Tornado %│
│ ☐Wind % ☐Hail %   ☑Reports        │
│ ▸ Local file                      │
└───────────────────────────────────┘
```

### 7.4 DATA tab

```
│ RADAR │ LAYERS │ SEVERE │DATA│ ⚙ │
├───────────────────────────────────┤
│ [<][Play][>] 9/10 [10▾]           │  ← shared transport (kept dup)
│ ▬▬▬▬▬▬●▬▬                         │
│ ── ARCHIVE ──────────────────────│
│ Fetch N scans [10] [+5 earlier]   │
│ ◀ [2026-06-09] ▶ [Today][List]    │
│ On click: (Loop) Single           │
│ 05 UTC  :02 :08 :14 :21 :27 …     │
│ 06 UTC  :02 :09 …                 │
│ Tornadoes (SPC) [Fetch]           │
│ 05:51Z EF3 Pleasant Hill, MO      │
│ ── LIVE FEEDS ───────────────────│
│ Poll URL [http://…    ][Feeds ▾]  │
│ [Start]  waiting for dir.list…    │
│ ── MODEL STORE ──────────────────│
│ HRRR 2026-06-09 21z · 3 hrs       │
│ [Download…]   → Model window      │
│ ── LOCAL ────────────────────────│
│ [Open radar file…]                │
└───────────────────────────────────┘
```

---

## Appendix A — code anchors @ 7ba98aa (fn names are the durable reference)

| Surface | Anchor | main.rs |
|---|---|---|
| Root layout | `impl eframe::App` → `fn ui` | 6161–6278 |
| Top bar / update chip | `top_bar` / `poll_update_check` | 6282 / 6406 |
| Sidebar shell / tabs | `side_panel` · `SidebarTab` · `SIDEBAR_TABS` · `sidebar_tab_bar` | 6420 · 2476 · 2483 · 6637 |
| Radar tab | `radar_controls_panel` | 6661 |
| Layers fold body (to become rail) | inline, 6827–7573: primary row 6849 · GOES 6889 · obs 6940 · GLM 7000 · SPC 7019 · model rows 7061 · OA block 7173 · Poll URL 7323 · placefiles 7378 · Add-layer 7470 | |
| Overlay rows | `radar_layers_panel` | 8199 |
| Row grammar | `layer_row` · `LayerRowSpec/Vis/Opacity` | 16140 · 16101–16128 |
| Archive tab | `archive_panel` | 5565 |
| Warnings tab | `hazard_panel` · `hazard_record_detail_lines` | 8454 · 16217 |
| Settings tab | `settings_panel` + `display_settings_section` / `hotkeys_section` / `model_settings_section` / `color_table_panel` / `stats_panel` | 6550 / 6478 / 6527 / 6598 / 8325 / 8543 |
| Loop transport | `frame_history_panel` | 8040 |
| Windows | `model_data_window` 10398 · `satellite_window` ~10750 · `wofs_window` ~12230 · `farm_window` 12452 · `vol3d_window` 12564 · Sounding inline in `ui` 6262 · `guide_window` guide.rs:57 |
| Status bar / map | `status_bar` 8996 · `single_pane_canvas` 9054 · `grid_canvas` 9315 · `best_radar_context_menu` 10133 |
| Keyboard | `handle_keyboard_navigation` 3759 · `handle_product_hotkeys` 3811 · defaults settings/src/lib.rs:89 |
| Constants | 134–143 (`PANEL_BUTTON_HEIGHT`, `SIDEBAR_*`) |
| Favorites (dormant) | settings/src/lib.rs:20 · written at main.rs:5419 · **read nowhere** |

**Salvaged from prior docs:** Direction A + row grammar + Add-layer front door + intent rules (proposal §3, landed); one-line status, section headers, hotkey prefixes, id_salt discipline, volume-gate placement (sidebar spec, landed). **Superseded:** the proposal's 10-step plan (5 steps shipped; the rest re-scoped above), its 5-tab `RADAR·LAYERS·ARCHIVE·WARN·⚙` split (ARCHIVE alone is too thin a tab now that Poll URL and model-store status need a home — DATA absorbs it), and both docs' line numbers.
