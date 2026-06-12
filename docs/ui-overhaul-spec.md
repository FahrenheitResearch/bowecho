# BowEcho UI Overhaul Spec v2 â€” "Finish the Layer Rail"

**Status:** supersedes `docs/ui-refresh-proposal.md` (v0.8.2 audit) and `docs/sidebar-redesign-spec.md` (v1 sidebar). Written against branch `fix/region-based-velocity-dealias` @ `7ba98aa` (v0.14.1, `crates/app_ui/src/main.rs` = 22,698 lines, eframe 0.34.3). Line numbers will drift; every reference names the function, which is durable.

**Read this first â€” what the audit actually found:**

1. `feat/ui-refresh` is **not stale â€” it is fully merged** (an ancestor of current HEAD). Old-proposal steps 2 (Model-button intent fix), 3 (`layer_row()` grammar + honest count), 4 (settings evicted from the fold), 5 (`+ Add layer â–¾`), and 8 (Download merged into the Model window) all shipped. Do not re-plan them; build on them.
2. Old-proposal steps that **never landed**: 1 (top-bar regroup + Sounding front door), 6 (LAYERS promoted to a tab), 7 (map-anchored warning card), 9 (viewport tear-off), 10 (session layout). These carry forward, re-scoped below.
3. The proposal's own prophecy â€” "the next spec must define where new layer types and new data windows GO, or v1.1 will accrete the same way" â€” **came true**. Since v0.8.2 the app grew: WoFS, FARM, 3D, Guide (4 more top-bar buttons â†’ 8 controls + a width-morphing live chip), and inside the Layers fold: a Surface-obs row with 3 inline sub-toggles, a bare-checkbox GLM row, an SPC row with a combo + 5 checkboxes, a 150-line OA/mesoanalysis workbench (Analyze obs / RAOB / Compute composites / two `â–¾` menus), and a Poll-URL acquisition row with a Feeds menu. The fold body inside `radar_controls_panel` is now ~750 lines and once again mixes three different kinds of thing: **layers** (rows), **compute** (OA workbench), and **acquisition** (Poll URL).
4. `AppSettings::favorites` is written (`remember_startup_site`, main.rs:5419) but **never read** â€” there is no favorites UI despite the data existing.
5. The Sounding window still has no front door: it opens only as a side effect of `poll_native_sounding` and once closed cannot be reopened without re-Alt-clicking.

**The verdict:** Direction A ("everything is a layer") was right and is half-built. The crowding is not density â€” GR2A users tolerate density â€” it is that one tab (RADAR) hosts four jobs and the top bar hosts seven windows. The fix is to **finish the rail, split the jobs into tabs, and give windows one scalable home**. No new dependencies, no docking, no first-run tour.

---

## 1. INFORMATION ARCHITECTURE â€” five tabs

`SidebarTab` (main.rs:2476) grows from 4 to 5 variants; Settings becomes a gear icon to keep â‰¥60 px per text tab at the 300 px minimum width:

```
RADAR Â· LAYERS Â· SEVERE Â· DATA Â· âš™
```

Every tab body is a `ScrollArea` with a stable `id_salt` (existing pattern in `side_panel`, main.rs:6420). Sections inside tabs are `CollapsingHeader`s with **fixed `id_salt`s and open-state mirrored into `AppSettings`** â€” eframe is built without the `persistence` feature (workspace `Cargo.toml:15`), so egui Memory does not survive restarts; add `sidebar_section_open: BTreeMap<String, bool>` to `AppSettings` and write it on change (same pattern as `save_overlay_defaults`).

### RADAR â€” operate the primary radar (volume-centric, mostly unchanged)

| Control | Current location | Disposition |
|---|---|---|
| Panes 1/2/4 + editing-pane notice | `radar_controls_panel` top (6677â€“6716) | keep, row 1 |
| SITE: site combo + Center | 6720â€“6744 | keep; **add favorites chip row** under the combo: small selectable chips from `app_settings.favorites` (finally reading the dormant field), each chip = one-click site switch + load-latest |
| Load Latest / Load Loop / Live / Chunks / Openâ€¦ | 6745â€“6784 | keep |
| One-line status + live-chunk readout + â–¸ Volume details | 6786â€“6825 | keep |
| **Layers fold (6827â€“7573)** | inline in `radar_controls_panel` | **REMOVE from this tab** â€” body becomes the LAYERS tab (Â§2). RADAR keeps a one-line link-row: `Layers: 7 (2 hidden) â†’` that switches to the LAYERS tab |
| PRODUCTS grid + VEL/SRV/hail contextual rows + Color + Hide-below + Gate filter | 7608â€“7870 | keep verbatim |
| TILT header + list | 7872â€“7936 | keep |
| LOOP (`frame_history_panel`) | 7938â€“7940 | keep |
| ALGORITHMS: Rotation markers, Storm tracks + SRVâ†tracks | 7942â€“7984 | keep here, **not** in the rail â€” they are volume-gated radar algorithms parameterized by radar state (storm motion), and their toggles belong next to the products they annotate. (They get rail rows only if they ever grow opacity/order needs.) |
| TOOLS: Inspectorâ€¦ menu, Inspector card, Vrot, Cross-section | 7986â€“8037 | keep; **the incoming RHI window's "arm RHI azimuth pick" control lands here** as a third armed tool, same checkbox + Clear pattern as Vrot/XS |

Net effect: the Radar tab loses its largest scroll cost (the fold) and becomes what its tooltip claims: "site, products, tilt, loop, algorithms â€” live operations".

### LAYERS â€” the rail (new tab; Â§2 is the full spec)

Everything drawn over the map, one row grammar, plus the `+ Add layer â–¾` front door and the OA analysis section (which *produces* layers).

### SEVERE â€” warnings + SPC in one place (rename of Warnings)

| Control | Current location | Disposition |
|---|---|---|
| Show / Active / Auto checkboxes | `hazard_panel` 8455â€“8459 | keep, rename "Active only" / "Auto-refresh" |
| Family filter wrap (TOR/SVR/FFW/â€¦) | 8460â€“8478 | keep |
| Fill slider | 8479â€“8486 | keep (also exposed as the warnings rail-row opacity â€” same state, two views) |
| Refresh Live / Clear | 8487â€“8498 | keep |
| Selected-hazard detail scroll | 8500â€“8513 | keep, **plus** the map-anchored warning card (old step 7, Â§6 PR-6): polygon click pops an `egui::Area` card at the click (event Â· expiry Â· hail/wind tags Â· "Full text â†’" link to this tab), reusing `hazard_record_detail_lines` (16217) |
| Summary scroll + â–¸ Local file | 8515â€“8540 | keep |
| **SPC config** (day combo + cat/torn/wind/hail + Reports) | currently jammed into one fold row (6019â€“7060) | moves here as section "SPC OUTLOOKS": day picker + kind checkboxes + Reports toggle. The rail shows only the two SPC rows (Â§2); their âš™ jumps here |

### DATA â€” acquisition and sources (Archive absorbs its siblings)

| Control | Current location | Disposition |
|---|---|---|
| Loop transport duplicate | `archive_panel` 5568 | keep (deliberate duplication, still correct) |
| Frames fetch-count + "+5 earlier" | 5570â€“5593 | keep; rename label "Frames" â†’ "Fetch N scans" (kills the two-frames-numbers confusion, old issue 1.3.9) |
| Date nav + volume list + On-click Loop/Single | 5594â€“5682 | keep |
| Tornadoes (SPC) fetch + report list | 5683â€“5739 | keep (it is archive-date-scoped event data, not a live severe layer) |
| **Poll URL + Feeds â–¾ + Start/Stop** | Layers fold 7323â€“7377 | **moves here** as section "LIVE FEEDS" â€” it is acquisition (it *replaces the primary volume source*), not a layer; the fold never should have held it |
| **Model store status + Fetch latest / Downloadâ€¦ link** | Model window Download fold (10398â€“10490) | window keeps the full panel; DATA gets a two-line "MODEL STORE" section: newest-run readout + one `Downloadâ€¦` button setting `model_dock_open + model_download_open` (the existing one-shot expand path) |
| Local radar file Openâ€¦ | duplicated from RADAR â–¸ SITE | second entry point here, same `start_local_volume_load` |

### âš™ (Settings) â€” unchanged content, icon tab

`settings_panel` (6550) keeps Display / Color tables / Hotkeys / Performance / Model sections verbatim. Tab label becomes the gear glyph with tooltip "Settings". The Hotkeys section is rewritten in Â§5's registry step to list *everything*, not just the number row.

---

## 2. THE LAYER RAIL

### 2.1 Row grammar v2

`layer_row` / `LayerRowSpec` (main.rs:16140 / 16121) already implements:

```
[vis] [name (hover=details)] [state-dot] [opacity â”€â”€â”€â”€] [trailingâ€¦]
```

Extend the spec struct â€” do not fork it â€” with two standardized slots:

```rust
struct LayerRowSpec<'a> {
    vis: LayerRowVis<'a>,          // Toggle | Badge (exists)
    name: &'a str,                 // exists
    name_width: f32,               // exists â€” tiers: 42 (site IDs) / 96 (standard) / 150 (placefiles)
    name_hover: &'a str,           // exists
    state: Option<&'a str>,        // exists (dot + hover)
    opacity: Option<LayerRowOpacity<'a>>, // exists (F32 | U8)
    order: Option<LayerRowOrder<'a>>,     // NEW: â†‘/â†“ â€” uniform reorder slot
    gear: Option<LayerRowGear<'a>>,       // NEW: âš™ â€” see contract below
}
```

- **Order**: standardized â†‘/â†“ small-buttons (the model rows' existing pattern, 7104â€“7119, generalized). **Deliberately not drag-and-drop in v1**: the priority persona operates a trackpad in a moving truck; two 18 px buttons beat a drag gesture under stress, and egui's built-in dnd can be layered on later without changing the spec. Reorder applies *within a rail group* (draw order is group-major, see 2.2).
- **Gear contract (the extensibility rule):** `âš™` opens the layer's **owning surface** â€” a window (`model_dock_open`, `show_satellite`, `wofs.open`, `farm.open`) or a tab section (SEVERE â–¸ SPC) â€” or, for layers with only 2â€“3 small options, an inline popover (`ui.menu_button` with the gear glyph). **A row may carry at most two inline extras besides âš™/âœ•**; everything else goes behind the gear. This single rule is what keeps the next five features from re-crowding the rail.
- `âœ•` stays a trailing extra where removal makes sense; the primary radar keeps the `â—‰` badge and no âœ•.

### 2.2 Rail structure â€” grouped, fixed order

Groups are weak uppercase mini-headers (NOT collapsing â€” the rail is one scannable list; collapsing returns the junk-drawer dynamics). Draw order on the map = bottom-to-top within the list, group-major:

```
BASE        primary radar, overlay radars
ATMOSPHERE  model fields, OA/composite fields, GOES, WoFS drape, FARM drape
OBS         surface obs, lightning (GLM)
SEVERE      SPC outlook, SPC reports, warnings
COMMUNITY   placefiles, (future: LSRs, spotter feeds)
```

### 2.3 Complete mapping â€” every existing toggle onto the rail

| Layer | Today (location) | Rail row spec |
|---|---|---|
| Primary radar | `layer_row` w/ Badge, fold 6849â€“6884 | unchanged; stays row 1 of BASE |
| Overlay radars | `radar_layers_panel` (8199) rows w/ Go/Ref/Pri/x | keep `layer_row`; inline extras = **Go** + **âœ•** (the storm-hop workflow earns inline); **Ref + Pri move behind âš™ popover**; "Overlays N + Clear" header line stays as the BASE group's right-aligned action |
| Model field layers | fold 7078â€“7172, â†‘/â†“/âš™/âœ• | unchanged semantics; â†‘/â†“ migrate to the `order` slot; the **Hour â—€ â–¶ stepper** moves from below the rows to the ATMOSPHERE group header (it steps all dock-following rows) |
| OA / composite fields | pushed into `model_layers` via `push_composite_layer` (7289â€“7291) | same rows as model fields, name suffix "(OA)" (already the behavior) â€” no special-casing |
| GOES | fold 6889â€“6939 (`layer_row` + âš™ + âœ•) | unchanged; âš™ â†’ Satellite window (already correct) |
| **WoFS drape** (incoming branch) | â€” | NEW row in ATMOSPHERE: vis toggle, name `WoFS <product>`, state = `<init>z+<min>` (hover: run/init/minute/sync), opacity F32, âš™ â†’ WoFS window, âœ• removes drape. The branch's "map-drape toggle" checkbox **must land as this row**, not as a window checkbox â€” the window keeps a "Show on radar map" button (the Sat/Model convention, 10800â€“10813) that *creates* the row |
| **FARM drape** (incoming branch) | â€” | NEW row in ATMOSPHERE: vis, name `FARM <sensor>`, state = live dot (`is_live()`), opacity, âš™ â†’ FARM window, âœ•. Same "Show on radar map creates the row" convention |
| Surface obs | fold 6940â€“6999 (`layer_row`, sub-toggles inline in trailing) | keep row; **METAR / Mesonet / adj-snd checkboxes move behind âš™ popover** (they violate the 2-extra budget today); state slot = `N stn Â· Xm` (already computed) |
| Lightning (GLM) | bare checkbox row 7000â€“7018 | **promote to `layer_row`**: vis = `glm_enabled`, name "Lightning", state = `N fl/10m`, no opacity v1 (age-fade is intrinsic), âš™ popover = satellite source pick (goes19/goes18, future) |
| SPC outlook | combo + 4 checkboxes + label, 7019â€“7048 | **one row**: vis = any kind enabled (toggling off disables all kinds, on restores last set), name `SPC D{n} outlook`, state = fetch spinner/age, no opacity (SPC's own colors), âš™ â†’ SEVERE â–¸ SPC OUTLOOKS section |
| SPC reports | checkbox in same row 7049 | own row: vis = `spc_reports_enabled`, name "SPC reports", âš™ â†’ SEVERE tab |
| Warnings | not in fold at all (tab-only) | NEW row: vis = `hazards_visible`, name "Warnings", state = active count, opacity = `hazard_fill_alpha` (U8, 0â€“80 range), âš™ â†’ SEVERE tab. Fixes "the warnings layer is invisible in the layer model" |
| Placefiles | fold 7378â€“7465 (`layer_row` + T/â†»/âœ•) | unchanged â€” T + â†» are exactly the 2-extra budget; URL input + Add stays as the COMMUNITY group footer |
| Poll URL | fold 7323â€“7377 | **NOT a layer** â†’ DATA tab (Â§1) |

### 2.4 `+ Add layer â–¾` (fold 7470â€“7572) â€” keep as the rail footer, grow it

Existing entries (Radar overlay â–¸ sites, Model fieldâ€¦, SpotterNetwork, Get model dataâ€¦, Satelliteâ€¦, Surface obs, Placefile URLâ€¦) carry over. Add:

- `WoFS drapeâ€¦` â†’ opens WoFS window (row born from its "Show on radar map")
- `FARM drapeâ€¦` â†’ opens FARM window (same)
- `Mesoanalysis (OA) â–¸` â†’ **this is the extensible home for the incoming composites-catalog branch**: a submenu organized like SPC's mesoanalysis page (Thermodynamics / Kinematics / Composite indices / â€¦), where each leaf either adds the cached field instantly (post-compute) or triggers the compute. The current flat `Composites â–¾` menu (7259â€“7280) migrates into this tree.
- The site-picker submenu gets a **favorites-first** ordering (read `app_settings.favorites`).

### 2.5 ANALYSIS (OA) â€” compute lives at the rail's bottom, not among rows

The OA workbench (fold 7173â€“7319: Analyze obs, RAOB sounding, Compute composites + progress, Derive (OA) â–¾) is *compute that emits layers*. It moves to a collapsing section **at the bottom of the LAYERS tab** named "ANALYSIS (OA)", default-closed, gated as today (`dock_has_field`). Rationale against burying it in the Model window: the analyst persona runs Analyze-obs during ops with the sidebar open; rationale against leaving it inline among rows: it is the single biggest reason the fold reads as a junk drawer. Its disabled-state hint strings ("â† turn on Surface obs above" etc., 7210â€“7218) are kept verbatim â€” they are the best self-explaining UI in the app.

---

## 3. TOP BAR (`top_bar`, main.rs:6282)

Today: `BowEcho | Reset View | Reload | Sat | Model | WoFS | FARM(â†’"<name> LIVE")| 3D | Guide | [update chip]` â€” eight controls with three different semantics, and the FARM button *changes width* when a sensor goes live (layout shift mid-ops).

Target:

```
BowEcho â”‚ Reset View Â· Reload â”‚Â·Â·Â·Â·Â·Â·Â·Â·Â·Â·Â·Â·Â·Â·â”‚ [DOW8 LIVE] [v0.15 â†‘] â”‚ Windows â–¾ Â· Guide
  brand    one-shot actions       (spacer)        status chips         menus (right)
```

- **Stays:** Reset View, Reload (one-shot actions, left). Guide (top-level, right â€” it is the discoverability anchor and must never be buried; the owner's no-tours rule makes Guide + hover text the *only* teaching surface).
- **Collapses into `Windows â–¾`** (a `menu_button`): Model data, Satellite, WoFS, FARM, 3D Volume, **Sounding** (new â€” `native_skewt_open` toggle, enabled iff `native_sounding.is_some()`; finally a front door), and the **incoming RHI window** lands here as entry #7 with zero top-bar churn. Each entry renders as a checked/unchecked toggle with its hotkey hint (Â§5). The Model entry keeps the intent rule (open â‡’ `model_enabled = true`, 6308â€“6314).
- **Chips, far right, fixed-width-reserved:** the FARM LIVE chip (green, click = open FARM window + `select_sensor(live_id)` â€” current behavior at 6325â€“6347, divorced from the window-toggle button) and the update-available chip (6364â€“6382, unchanged). Chips are *status*, buttons are *commands*; they no longer share widgets.
- Rationale for a menu over an icon strip: window count is 7 going on 9 (RHI, future obs-sounding browser); a strip re-crowds within two releases, and every window also remains reachable from its rail-row âš™ and its hotkey â€” the menu is the third path, not the only one. GR2A precedent: deep dialogs live in menus there too.

---

## 4. DENSITY RULES

Extract a `mod ui_theme` (new file `crates/app_ui/src/ui_theme.rs`) holding the existing magic numbers (main.rs:134â€“143) plus the new contract, and have `configure_style` (2501) read from it:

| Constant | Value | Use |
|---|---|---|
| `ROW_H` (= `PANEL_BUTTON_HEIGHT`) | 24.0 | every button/row/slider height â€” no exceptions |
| `ROW_SPACING_X` | 3.0 | inside layer rows + tab bar (already the convention) |
| `SECTION_SPACING` | 8.0 + separator | `section_header` (6466) â€” keep |
| `SUBHEAD_COLOR` | rgb(148,160,172) | section + rail-group headers (matches guide.rs:12) |
| `ACCENT_COLOR` | rgb(120,168,220) | editing-pane notice, keycap hints (guide.rs:14) |
| `LIVE_COLOR` | rgb(110,245,130) | live chips/dots only â€” never decorative |
| `COMBO_MAX_W` | 220.0 | no combo wider (site combo fills, capped) |
| `NAME_W_SITE / STD / WIDE` | 42 / 96 / 150 | the three `name_width` tiers â€” pick one, never a fourth |
| Sidebar | 300â€“560, default 380 | unchanged (`SIDEBAR_*_WIDTH`) |

**Icons vs labels:** glyph-only is allowed exactly for the universal set already in use â€” `â†‘ â†“ âœ• âš™ â†» â—€ â–¶ â¸ â—‰` â€” and each MUST carry `on_hover_text`. Everything else is a text label. Buttons that exist in both a window and the rail use identical strings ("Show on radar map") so the Guide can name them once.

**Discoverability doctrine (no tours, ever):** every interactive control has hover text; hover text names its hotkey where one exists ("hotkey 3" pattern, 7626â€“7628); the Guide (guide.rs) gains a "Layers" section in PR-4 and its Shortcuts section is generated from the keybinding registry in PR-7 so docs cannot drift from bindings. One-line status + hover-for-detail stays the law (R1 status line is the model).

---

## 5. KEYBOARD MAP â€” conflicts check and additions

**Existing bindings (preserve byte-for-byte; all routed through `text_edit_focused()` + `consume_key`, `handle_keyboard_navigation` 3759):**

| Binding | Action | Anchor |
|---|---|---|
| `1â€“9, 0` | products (remappable; REF VEL SRV RHO ZDR SW CREF ET VIL VILD) | `handle_product_hotkeys` 3811; defaults settings/src/lib.rs:89 |
| `â†/â†’` | step product (focused pane) | 3769â€“3787 |
| `â†‘/â†“` | step tilt (focused pane) | 3789â€“3806 |
| Shift+click | pin/release inspector | 9145 |
| Alt+click / Ctrl+Alt hover | model sounding / follow-mouse | 9222â€“9227 |
| Ctrl+click (no Alt/Shift) | switch to lowest-beam radar | 9272â€“9283 |
| Right-click | best-radar menu / clear armed tool | 9101â€“9106, 9269 |

**Conflict findings:**
1. Number row is consumed only when a volume exists (`handle_product_hotkeys` early-returns, 3824) â€” new no-volume bindings on digits would be ambiguous; **do not bind digits to anything else**.
2. `Ctrl+1..4` (pane focus, proposed) does not collide â€” product keys consume `Modifiers::NONE` only. Safe.
3. `Space` collides with egui's button activation when a widget has keyboard focus; gate on `ctx.memory(|m| m.focused().is_none())` in addition to `text_edit_focused`.
4. Arrow keys vs egui slider focus: already handled by the consume-key-at-frame-start order (keyboard handler runs before panels, 6225). Keep that ordering for all new keys.
5. The GR2A muscle-memory conflict (`â†/â†’` = frames there) stands: ship the `key_profile: "bowecho" | "gr2a"` setting (old Â§5.2), surfaced in Guide â–¸ Shortcuts, not as a tour.

**New bindings (PR-7, all remappable):** `Space` play/pause loop Â· `,`/`.` frame step Â· `Esc` disarm tool â†’ close warning card â†’ close topmost window Â· `X` cross-section Â· `R` Vrot Â· `I` inspector card Â· `L` LAYERS tab toggle Â· `W` SEVERE tab toggle Â· `Ctrl+1..4` pane focus Â· `F1` Guide Â· `Ctrl+M/S/F/O/D/3` window toggles (Model/Sat/FARM/WoFS soundings/3D â€” exact letters decided in PR-7, shown in the Windows â–¾ menu). Implementation: generalize to `key_bindings: BTreeMap<String,String>` (action-id â†’ key) in `AppSettings`, leaving `product_hotkeys` untouched for back-compat; Settings â–¸ Hotkeys and Guide â–¸ Shortcuts both render the registry.

---

## 6. MIGRATION PLAN â€” eight PR-sized steps

Ordering principle: **scaffold the homes before the five feature branches land**, so they merge into the new structure instead of bolting onto the fold and being migrated twice. Every step compiles and ships alone. Coordination gate (same as both prior specs): confirm no concurrent app_ui session (the `rra-review` clone's UI agent), branch off current mainline, never touch dealias/render-worker code.

| PR | Scope | Exact moves | Feature-branch gates |
|---|---|---|---|
| **1** | **Top bar: Windows â–¾ + chips + Sounding front door.** Rebuild `top_bar` (6282): left actions; `Windows â–¾` menu (Sat/Model/WoFS/FARM/3D/Sounding toggles, Model keeps intent rule); FARM LIVE + update chips right-aligned fixed slots; Guide stays top-level. | edit `top_bar`; new helper `fn windows_menu(&mut self, ui)` | **Land before everything.** RHI branch then adds one menu entry â€” trivial merge |
| **2** | **Row grammar v2.** Add `order`/`gear` slots to `LayerRowSpec` (16121) + `layer_row` (16140); port GLM (7000) and SPC (7019) onto `layer_row`; add the Warnings row; move obs sub-toggles + overlay Ref/Pri behind âš™ popovers; model-row â†‘/â†“ â†’ `order` slot. All **in place inside the existing fold** â€” zero relocation. | `layer_row`, `radar_layers_panel`, fold body | drape branches SHOULD wait for this (their rows use v2) |
| **3** | **Extraction, no movement.** Split `radar_controls_panel` (6661): `fn layers_rail(&mut self, ui, ctx)` (fold body minus the two evictions), `fn live_feeds_section` (Poll URL block 7323â€“7377), `fn oa_analysis_section` (7173â€“7319), `fn add_layer_menu` (7470â€“7572). Call sites unchanged â€” the fold still renders in RADAR. Pure churn-minimizer. | new fns, same output | **Merge point A: WoFS-drape + FARM-drape branches land here** â€” each adds one row to `layers_rail` + a "Show on radar map" button in its window, per Â§2.3 row specs. WoFS sounding-station-picker branch is window-internal: lands any time |
| **4** | **Tab promotion.** `SidebarTab` (2476) â†’ `{Radar, Layers, Severe, Data, Settings}`; `SIDEBAR_TABS` (2483) + tooltips (2490); `side_panel` match (6425) routes LAYERS â†’ `layers_rail` + `add_layer_menu` + `oa_analysis_section`, DATA â†’ `archive_panel` + `live_feeds_section` + model-store section; RADAR gets the "Layers: N â†’" link-row; Settings tab label â†’ âš™ glyph. Section open-states mirrored into `AppSettings.sidebar_section_open`. Guide gains a "Layers" section. | `SidebarTab`, `side_panel`, `archive_panel`, guide.rs | do not start until A is merged (conflict surface = the fold) |
| **5** | **Favorites + rail polish.** Favorites chip row in RADAR â–¸ SITE + favorites-first site lists (read `app_settings.favorites`; add remove-affordance on right-click); rail group headers + group-major draw order; Hour stepper to ATMOSPHERE header. | `radar_controls_panel`, `layers_rail`, layer compositor draw order | â€” |
| **6** | **SEVERE consolidation + warning card.** SPC config section into SEVERE; rail SPC/Warnings âš™ jump there; map-anchored polygon card (`egui::Area` at click, `hazard_record_detail_lines`), behind a Settings checkbox if nervous. | `hazard_panel`, new `fn warning_card_overlay` | â€” |
| **7** | **Keyboard registry.** `key_bindings` map in `AppSettings`; new bindings per Â§5; Settings â–¸ Hotkeys renders full registry; Guide â–¸ Shortcuts generated from it; Windows â–¾ entries show their keys. Keyboard-neutral until this PR â€” steps 1â€“6 add no keys. | `handle_keyboard_navigation`, settings lib, `hotkeys_section`, guide.rs | **Merge point B: composites-catalog branch** lands here or after â€” its menu tree slots into `add_layer_menu â–¸ Mesoanalysis (OA)` (Â§2.4) and `oa_analysis_section` |
| **8** | **Session layout + viewport tear-off (stretch).** Persist open windows / tab / pane grid / layer set in config.json; then `ctx.show_viewport_deferred` Detach for Sounding â†’ Sat â†’ Model (old step 9; the only platform-QA step â€” texture sharing + repaint wakeups on Windows/macOS). Cut this PR before cutting anything above it. | `AppSettings`, window fns | post-v1 candidate |

---

## 7. WIREFRAMES

### 7.1 Default storm view (1Ã— pane, volume loaded)

```
â”Œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”
â”‚ BowEcho â”‚ Reset View Â· Reload          [DOW8 LIVE] [v0.15â†‘]  Windows â–¾ Â· Guideâ”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¬â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚                                                     â”‚ RADAR LAYERS SEVERE    â”‚
â”‚                                                     â”‚       DATA  âš™          â”‚
â”‚                 MAP CANVAS                          â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚        (colorbar Â· mode chip Â· inspector)           â”‚ Panes  1 2 4           â”‚
â”‚                                                     â”‚ â”€â”€ SITE â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”‚
â”‚                                                     â”‚ [KTLX â€” Oklahomaâ€¦ â–¾][Center]
â”‚                                                     â”‚ â˜…KTLX â˜…KEAX â˜…KFDR     â”‚ â† favorites chips
â”‚                                                     â”‚ [Load Latest][Load Loop]â”‚
â”‚                                                     â”‚ â˜Live â˜Chunks [Openâ€¦] â”‚
â”‚                                                     â”‚ KTLX Â· VCP 212 Â· 22:41Z Â· 14 cuts
â”‚                                                     â”‚ Layers: 7 (2 hidden) â†’ â”‚ â† link to LAYERS
â”‚                                                     â”‚ â”€â”€ PRODUCTS â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”‚
â”‚                                                     â”‚ 1Â·REF 2Â·VEL 3Â·SRV 4Â·RHOâ”‚
â”‚                                                     â”‚ 5Â·ZDR 6Â·SW 7Â·CREF â€¦   â”‚
â”‚                                                     â”‚ â˜‘Unfold [Region â–¾] â˜Flipâ”‚
â”‚                                                     â”‚ Color [NWS Velocity â–¾] Editâ€¦
â”‚                                                     â”‚ â˜Hide |val| below      â”‚
â”‚                                                     â”‚ â˜Gate filter           â”‚
â”‚                                                     â”‚ â”€â”€ TILT â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”‚
â”‚                                                     â”‚ â†‘/â†“   #00 0.48Â° 720 â€¦  â”‚
â”‚                                                     â”‚ â”€â”€ LOOP â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”‚
â”‚                                                     â”‚ [<][Pause][>] 9/10 [10â–¾]
â”‚                                                     â”‚ â–¬â–¬â–¬â–¬â–¬â–¬â–¬â—â–¬              â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤ â”€â”€ ALGORITHMS â”€â”€â”€â”€â”€â”€â”€â”€â”‚
â”‚ Rendering KTLXâ€¦ â”‚      9 frames Â· 1 overlay Â· 230 kmâ”‚ â˜‘Rotation â˜‘Tracks      â”‚
â””â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”´â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜
```

### 7.2 LAYERS tab (rail expanded)

```
â”‚ RADAR â”‚LAYERSâ”‚ SEVERE â”‚ DATA â”‚ âš™ â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚ BASE                    Clear     â”‚
â”‚ â—‰ KTLX REF 0.5Â°   liveâ— â–“â–“â–“â–“â–“â–‘    â”‚  â† primary: badge, no âœ•
â”‚ â˜‘ KEAX            liveâ— â–“â–“â–“â–‘â–‘ [Go][âš™][âœ•]      âš™: Refresh Â· Make primary
â”‚ ATMOSPHERE              Hour â—€ â–¶  â”‚  â† stepper on group header
â”‚ â˜‘ REFC f02        12zâ—  â–“â–“â–“â–“â–‘ [â†‘][â†“][âš™][âœ•]    âš™ â†’ Model window
â”‚ â˜‘ SBCAPE (OA) f02  OAâ—  â–“â–“â–‘â–‘â–‘ [â†‘][â†“][âš™][âœ•]
â”‚ â˜‘ GOES-19 C13           â–“â–“â–‘â–‘â–‘ [âš™][âœ•]          âš™ â†’ Sat window
â”‚ â˜‘ WoFS UH-paint  21z+45 â–“â–“â–“â–‘â–‘ [âš™][âœ•]          âš™ â†’ WoFS window   (incoming)
â”‚ â˜‘ FARM DOW8      liveâ—  â–“â–“â–“â–“â–‘ [âš™][âœ•]          âš™ â†’ FARM window   (incoming)
â”‚ OBS                               â”‚
â”‚ â˜‘ Surface obs  312stnÂ·3m      [âš™] â”‚  âš™: â˜‘METAR â˜‘Mesonet â˜adj snd
â”‚ â˜‘ Lightning    47 fl/10m      [âš™] â”‚
â”‚ SEVERE                            â”‚
â”‚ â˜‘ SPC D1 outlook  âœ“â—          [âš™] â”‚  âš™ â†’ SEVERE tab
â”‚ â˜‘ SPC reports                 [âš™] â”‚
â”‚ â˜‘ Warnings  12 act  fillâ–“â–‘    [âš™] â”‚
â”‚ COMMUNITY                         â”‚
â”‚ â˜‘ SpotterNetwork    [T][â†»][âœ•]     â”‚
â”‚ [https://â€¦ placefile URL  ][Add]  â”‚
â”‚                                   â”‚
â”‚ [+ Add layer â–¾]                   â”‚  Radar overlay â–¸ Â· Model fieldâ€¦ Â·
â”‚                                   â”‚  WoFS drapeâ€¦ Â· FARM drapeâ€¦ Â·
â”‚                                   â”‚  Mesoanalysis (OA) â–¸ â–¸ Composite indices â–¸ STP(eff)â€¦
â”‚ â–¸ ANALYSIS (OA)                   â”‚  Satelliteâ€¦ Â· Surface obs Â· Placefile URLâ€¦
â”‚   [Analyze obs] [Obs sounding]    â”‚
â”‚   [Compute composites] 412/1024 âŸ³ â”‚
â””â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜
```

### 7.3 SEVERE tab

```
â”‚ RADAR â”‚ LAYERS â”‚SEVEREâ”‚ DATA â”‚ âš™ â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚ â˜‘Show â˜‘Active only â˜Auto-refresh  â”‚
â”‚ â˜‘TOR â˜‘SVR â˜‘FFW â˜‘Flood â˜SMW â˜SQW   â”‚
â”‚ â˜‘Watch â˜‘MD â˜SPS                   â”‚
â”‚ Fill â–“â–“â–‘â–‘â–‘â–‘â–‘â–‘ 28                  â”‚
â”‚ [Refresh Live] [Clear]            â”‚
â”‚ â”Œ selected â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”  â”‚
â”‚ â”‚ Tornado Warning  KTLX 22:58Z â”‚  â”‚  â† also pops as map card
â”‚ â”‚ â€¦radar-indicated, 70mph hailâ€¦â”‚  â”‚     at the polygon (PR-6)
â”‚ â””â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜  â”‚
â”‚ 84 scanned Â· 12 polygons Â· live   â”‚
â”‚ â”€â”€ SPC OUTLOOKS â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”‚
â”‚ Day [D1 â–¾] â˜‘Categorical â˜‘Tornado %â”‚
â”‚ â˜Wind % â˜Hail %   â˜‘Reports        â”‚
â”‚ â–¸ Local file                      â”‚
â””â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜
```

### 7.4 DATA tab

```
â”‚ RADAR â”‚ LAYERS â”‚ SEVERE â”‚DATAâ”‚ âš™ â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚ [<][Play][>] 9/10 [10â–¾]           â”‚  â† shared transport (kept dup)
â”‚ â–¬â–¬â–¬â–¬â–¬â–¬â—â–¬â–¬                         â”‚
â”‚ â”€â”€ ARCHIVE â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”‚
â”‚ Fetch N scans [10] [+5 earlier]   â”‚
â”‚ â—€ [2026-06-09] â–¶ [Today][List]    â”‚
â”‚ On click: (Loop) Single           â”‚
â”‚ 05 UTC  :02 :08 :14 :21 :27 â€¦     â”‚
â”‚ 06 UTC  :02 :09 â€¦                 â”‚
â”‚ Tornadoes (SPC) [Fetch]           â”‚
â”‚ 05:51Z EF3 Pleasant Hill, MO      â”‚
â”‚ â”€â”€ LIVE FEEDS â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”‚
â”‚ Poll URL [http://â€¦    ][Feeds â–¾]  â”‚
â”‚ [Start]  waiting for dir.listâ€¦    â”‚
â”‚ â”€â”€ MODEL STORE â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”‚
â”‚ HRRR 2026-06-09 21z Â· 3 hrs       â”‚
â”‚ [Downloadâ€¦]   â†’ Model window      â”‚
â”‚ â”€â”€ LOCAL â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”‚
â”‚ [Open radar fileâ€¦]                â”‚
â””â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜
```

---

## Appendix A â€” code anchors @ 7ba98aa (fn names are the durable reference)

| Surface | Anchor | main.rs |
|---|---|---|
| Root layout | `impl eframe::App` â†’ `fn ui` | 6161â€“6278 |
| Top bar / update chip | `top_bar` / `poll_update_check` | 6282 / 6406 |
| Sidebar shell / tabs | `side_panel` Â· `SidebarTab` Â· `SIDEBAR_TABS` Â· `sidebar_tab_bar` | 6420 Â· 2476 Â· 2483 Â· 6637 |
| Radar tab | `radar_controls_panel` | 6661 |
| Layers fold body (to become rail) | inline, 6827â€“7573: primary row 6849 Â· GOES 6889 Â· obs 6940 Â· GLM 7000 Â· SPC 7019 Â· model rows 7061 Â· OA block 7173 Â· Poll URL 7323 Â· placefiles 7378 Â· Add-layer 7470 | |
| Overlay rows | `radar_layers_panel` | 8199 |
| Row grammar | `layer_row` Â· `LayerRowSpec/Vis/Opacity` | 16140 Â· 16101â€“16128 |
| Archive tab | `archive_panel` | 5565 |
| Warnings tab | `hazard_panel` Â· `hazard_record_detail_lines` | 8454 Â· 16217 |
| Settings tab | `settings_panel` + `display_settings_section` / `hotkeys_section` / `model_settings_section` / `color_table_panel` / `stats_panel` | 6550 / 6478 / 6527 / 6598 / 8325 / 8543 |
| Loop transport | `frame_history_panel` | 8040 |
| Windows | `model_data_window` 10398 Â· `satellite_window` ~10750 Â· `wofs_window` ~12230 Â· `farm_window` 12452 Â· `vol3d_window` 12564 Â· Sounding inline in `ui` 6262 Â· `guide_window` guide.rs:57 |
| Status bar / map | `status_bar` 8996 Â· `single_pane_canvas` 9054 Â· `grid_canvas` 9315 Â· `best_radar_context_menu` 10133 |
| Keyboard | `handle_keyboard_navigation` 3759 Â· `handle_product_hotkeys` 3811 Â· defaults settings/src/lib.rs:89 |
| Constants | 134â€“143 (`PANEL_BUTTON_HEIGHT`, `SIDEBAR_*`) |
| Favorites (dormant) | settings/src/lib.rs:20 Â· written at main.rs:5419 Â· **read nowhere** |

**Salvaged from prior docs:** Direction A + row grammar + Add-layer front door + intent rules (proposal Â§3, landed); one-line status, section headers, hotkey prefixes, id_salt discipline, volume-gate placement (sidebar spec, landed). **Superseded:** the proposal's 10-step plan (5 steps shipped; the rest re-scoped above), its 5-tab `RADARÂ·LAYERSÂ·ARCHIVEÂ·WARNÂ·âš™` split (ARCHIVE alone is too thin a tab now that Poll URL and model-store status need a home â€” DATA absorbs it), and both docs' line numbers.