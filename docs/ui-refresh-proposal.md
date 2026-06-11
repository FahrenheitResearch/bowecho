# BowEcho UI Refresh Proposal (pre-v1.0)

**Status:** proposal / spec seed — to be executed after the current feature arc, before v1.0.
**Audited at:** branch `fix/region-based-velocity-dealias`, commit `0781794` ("v0.8.2"), `crates/app_ui/src/main.rs` = 19,010 lines, eframe **0.34.3**.
**Prior art:** `docs/sidebar-redesign-spec.md` (the last redesign). Line numbers below are accurate at this commit but WILL drift — every reference also names the function, which is the durable anchor.

The app grew from "fast L2 viewer" into an atmosphere workstation (radar + model layers + GOES + native soundings + hail/wind science) across rapid arcs. Each arc bolted its UI onto whatever surface was nearest. This document is the relook: what exists, what's inconsistent, what the peers do, three candidate directions, one recommendation, and a migration plan whose every step ships alone.

---

## 1. AUDIT

### 1.1 Information architecture today (the full tree)

```
APP WINDOW
├─ TOP BAR (top_bar, main.rs:5899; 42px)
│   ├─ "BowEcho" heading │ separator
│   ├─ [Reset View]  [Reload]            ← actions
│   ├─ [Sat]   toggle → "Satellite (GOES)" window
│   ├─ [Model] toggle → "Model data" window
│   └─ (planned: [Guide])
│
├─ RIGHT SIDEBAR (side_panel, main.rs:5926; 300–560px, default 380)
│   ├─ Tab bar: RADAR · ARCHIVE · WARNINGS · SETTINGS (SidebarTab, main.rs:2233)
│   │
│   ├─ RADAR tab (radar_controls_panel, main.rs:6130)
│   │   ├─ Panes row (1/2/4) + "Editing pane N" notice
│   │   ├─ SITE: site combo + Center │ Load Latest · Load Loop · ☐Live · ☐Chunks
│   │   │        one-line status │ live-chunk readout │ ▸ Volume details
│   │   ├─ ▸ Layers (N)  ←———— THE JUNK DRAWER (see 1.3.1)
│   │   │   ├─ radar_layers_panel (main.rs:7150): "Overlays N" + Clear;
│   │   │   │   per-overlay row: ☐vis · site · state-dot · opacity · Go/Ref/Pri/x
│   │   │   ├─ ☐ "Model data" master switch  (kills dock/LUT/soundings app-wide)
│   │   │   ├─ "Keep runs" DragValue         (model store DISK RETENTION policy)
│   │   │   ├─ "Radar" opacity slider        (PRIMARY radar opacity)
│   │   │   ├─ GOES row (if sat layer):  ☐vis · "GOES" · opacity · ✕
│   │   │   ├─ Model row (if model layer): ☐vis · ◀ · "Model: var (units)" · ▶ · opacity · ✕
│   │   │   ├─ freshness line + [Fetch latest] + [Download…]→Model download WINDOW + spinner/✕
│   │   │   └─ Placefiles: URL input + Add; per-slot ☐ · title · ↻ · ✕
│   │   ├─ ——— VOLUME GATE (everything below needs a volume) ———
│   │   ├─ PRODUCTS: hotkey-prefixed grid ("1·REF"…)
│   │   │   ├─ contextual row (≤1): VEL (Unfold+engine+Flip) | SRV (Motion+←tracks)
│   │   │   │                       | MEHS/POSH/POH (Hail 0°/−20° + From HRRR)
│   │   │   ├─ Color row + [Edit…]→Settings▸Color tables (one-shot open)
│   │   │   ├─ Hide-below row (per-family display threshold)
│   │   │   └─ Gate filter row (GR2-style co-located REF threshold)
│   │   ├─ TILT: ↑/↓ hint · follow-main · tilt list (168px scroll)
│   │   ├─ LOOP: frame_history_panel (main.rs:6991): transport · scrub · status · ▸Frames(N)
│   │   ├─ ALGORITHMS: ☐Rotation markers │ ☐Storm tracks + [SRV←tracks]
│   │   └─ TOOLS: [Inspector…]menu · ☐Inspector card · ☐Vrot tool+Clear · ☐Cross-section+Clear XS
│   │
│   ├─ ARCHIVE tab (archive_panel, main.rs:5223)
│   │   ├─ frame_history_panel AGAIN (deliberate duplication, see 1.5)
│   │   ├─ "Frames" fetch-count + [+5 earlier]
│   │   ├─ Date nav: ◀ · YYYY-MM-DD · ▶ · Today · List · spinner
│   │   ├─ On click: Loop|Single
│   │   ├─ Volume list (hour headers + minute chips, 190px scroll)
│   │   └─ Tornadoes (SPC): Fetch + report list (click = jump to lowest-beam radar + loop)
│   │
│   ├─ WARNINGS tab (hazard_panel, main.rs:7397)
│   │   ├─ ☐Show ☐Active ☐Auto │ family filter wrap (TOR/SVR/FFW/…/MD/SPS)
│   │   ├─ Fill slider │ [Refresh Live] [Clear]
│   │   ├─ selected-hazard detail scroll (only visible if you're ON this tab)
│   │   ├─ summary scroll
│   │   └─ ▸ Local file
│   │
│   └─ SETTINGS tab (settings_panel, main.rs:6056)
│       ├─ ▸ Display (open): ☐Smooth · Basemap combo · ☐Bold town labels
│       ├─ ▸ Color tables: family target + Current · built-ins · path/Browse/Load/Reset · status
│       ├─ ▸ Hotkeys: arrow hint + number-row table + config path
│       └─ ▸ Performance: ☐Details + timing labels
│
├─ BOTTOM: status bar (status_bar, main.rs:7802; 30px)
│   └─ status slot │ frame status · overlay count · range · map scale · cursor readout
├─ BOTTOM: cross-section panel (main.rs:10903; appears when armed/active, 120–520px)
│
├─ FLOATING egui::Window-s (all over the map canvas)
│   ├─ "Model data" (main.rs:5863; 1080×660) — rw_ui dock: Runs browser (left panel)
│   │     · field viewer (center, + [Show on radar map]) · Sounding panel (right, on demand)
│   ├─ "Model download" (model_download_window, main.rs:9085; 560×560) — rw_ui DownloadPanel:
│   │     date/cycle/hours/profile + live size estimate + per-hour progress
│   ├─ "Satellite (GOES)" (satellite_window, main.rs:9385; 900×700) —
│   │     ▸ Live follow (rw_ui SatellitePanel: band/sector/cadence + follow status)
│   │     · [Show on radar map] · sat_player frame playback
│   └─ "Sounding (native)" (main.rs:5881; 1265×950) — sounding_panels::draw_full:
│         skew-T + hodograph + slinky + parameter table. Opens itself when an
│         Alt+click sounding lands (poll_native_sounding, main.rs:9011)
│
└─ MAP CANVAS (map_canvas, main.rs:7837) — invisible interaction surface
    ├─ drag pan · wheel zoom · pane-focus click (grid mode)
    ├─ right-click → "lowest beam here" context menu (best_radar_context_menu, main.rs:8829)
    ├─ Shift+click → pin/release inspector card
    ├─ Alt+click → model sounding · Ctrl+Alt+hover → live follow-the-mouse sounding
    ├─ armed tools own clicks: Vrot (2 clicks), Cross-section (A→B)
    └─ chrome: colorbar · mode chip · raw-VEL tag · site markers · inspector card
```

### 1.2 The last redesign's principles — and the drift since

`docs/sidebar-redesign-spec.md` established, verbatim or in spirit:

| Principle (v1 sidebar spec) | Status today |
|---|---|
| **3 tabs** (Radar/Warnings/Settings), GR2A order | Drifted: 4 tabs (Archive added — justified, but unplanned) |
| **"No egui::Window — everything stays in the existing right panel"** | Abandoned silently: FOUR floating windows now exist, with no stated rule for what earns a window |
| Layers fold = "things drawn on the map from elsewhere" (overlays + placefiles) | Drifted into a junk drawer: model master switch, disk retention, primary-radar opacity, ingest buttons, a window launcher |
| One-line status, hover for detail | Held ✓ |
| Section headers (SITE/PRODUCTS/TILT/LOOP/ALGORITHMS/TOOLS) | Held ✓ (TOOLS grew: Inspector menu, Vrot) |
| Hotkey-prefixed product grid | Held ✓ (good — keep) |
| Quick color picker ↔ Settings manager linked by Edit… | Held ✓ |
| Contextual rows: ≤1 family block, no reflow | Held ✓, plus Gate filter row appended (consistent) |

The drift is not sloppiness — each arc (model data, satellite, soundings) needed surface area the sidebar couldn't give. The lesson for this refresh: **the next spec must define where new layer types and new data windows GO**, or v1.1 will accrete the same way.

### 1.3 Inconsistencies

1. **Model controls are split across FOUR surfaces with no rule.** Master enable, disk retention ("Keep runs"), freshness readout, [Fetch latest], and the [Download…] launcher live in the *Layers fold* (main.rs:6283–6464); run/field browsing and soundings live in the *Model data window*; acquisition specs live in the *Model download window*; the window toggle lives in the *top bar*. A user asking "where do I do model things?" has four answers, and the answer "Layers fold" contains a *settings-class disk policy* (Keep runs) that belongs in Settings.
2. **Satellite is split three ways and is not a peer of model.** Acquisition + playback in the Satellite window; the map layer row + opacity in the Layers fold; the toggle in the top bar. But unlike model there is no master switch, no freshness line in Layers, and no retention control in the sidebar. Two layer types, two ad-hoc layouts.
3. **"Layers (N)" lies.** `layer_count = radar_layers + enabled placefiles` (main.rs:6276–6277) — the GOES and model layers render inside the fold but are not counted. Cosmetic, but it's the tell that layer rows were added by different arcs without a shared row model.
4. **Windows vs tabs has no rule.** Warnings = tab, Satellite = window. Archive browsing = tab, model-run browsing = window. Deep config (Model download) = window, deep config (color tables) = Settings tab section. The honest current rule is "whatever the arc found easiest."
5. **The primary radar is not a layer.** Its opacity slider sits in the fold labeled just "Radar" (main.rs:6323–6335) with none of the row furniture (no vis toggle, no status dot, no site label) that overlay radars get two rows above. New users read it as a global dimmer; it is, but only because the primary isn't modeled as a row.
6. **Top-bar buttons mix semantics.** [Reset View][Reload] are one-shot actions; [Sat][Model] are window toggles (selectable_label state). No grouping, no separator, and the planned [Guide] will make five unaligned buttons.
7. **Master switch vs top bar fight (bug-class).** With "Model data" unchecked, clicking top-bar [Model] sets `model_dock_open = true` and creates the dock at end-of-frame (main.rs:5857–5874); next frame `poll_model_layer` (main.rs:9577) sees `!model_enabled` + dock present, tears it down and forces `model_dock_open = false`. The window flashes for one frame and dies, with no message. The toggle and the master switch are two bools that disagree.
8. **Warning detail is tab-gated.** Clicking a polygon on the map selects it, but the detail text renders only inside the Warnings tab (main.rs:7443–7456). During warning ops — exactly when you're staring at the map — you must tab away from Radar controls to read the text. The last spec flagged this (its optional item 10) and it was never implemented.
9. **Two "frames" numbers a row apart in Archive.** `archive_frame_count` ("Frames", fetch size, main.rs:5229) vs `history_frame_limit` (the "N frames" combo inside the duplicated transport directly above it). Different concepts, adjacent positions, near-identical names.
10. **The Sounding window has no front door.** It opens only as a side effect of an Alt+click sounding landing (main.rs:9068). Close it and there is no button anywhere to reopen the last sounding — you must re-Alt+click the map. Every other window has a toggle.
11. **Tool arming is a checkbox at the bottom of a long scroll.** Vrot and Cross-section live in TOOLS, the last section of the Radar tab — on a 768px-tall laptop the arm checkbox is below the fold of a tab you may not even be on (e.g. Warnings during ops). The map mode chip shows armed state, but arming/disarming requires sidebar travel. No hotkeys.
12. **The Inspector uses a menu-button config pattern used nowhere else** ([Inspector…] menu_button, main.rs:6939) — fine in isolation, but it's a third config idiom alongside folds and windows.

### 1.4 Discoverability dead zones

- **Alt+click / Ctrl+Alt follow-mouse soundings** — the single most impressive feature in the app is documented only inside two hover tooltips (the master-switch checkbox and the model layer row vis toggle). A GR2A native will never find it.
- **Shift+click inspector pinning** — tooltip-only, on a checkbox in bottom-of-scroll TOOLS.
- **Right-click "lowest beam here"** — invisible until accidentally triggered; it's also the *best* site-switching UX in the app.
- **"Show on radar map"** — the only path from Satellite/Model windows to the map is a small button mid-window. Users report layers as "not working" when they've loaded a field but never found this button. (Predicted, not reported — but it will be.)
- **The hotkey reference** lives in Settings ▸ Hotkeys (default-closed) and lists only the number row, not the arrows, not the click modifiers, not right-click behaviors.
- **Vrot/Cross-section/Inspector** capabilities are tooltips on checkboxes. The incoming Guide button is the right instinct; this refresh should give it a real cheat-sheet to show.

### 1.5 Redundancies (mostly defensible — keep, but know them)

- `frame_history_panel` renders in both Radar▸LOOP and the top of Archive (main.rs:5226). Deliberate ("archive browsing shouldn't need a tab switch to play what it loaded") and correct; the cost is Archive's first 5 rows duplicating Radar's. Kept in all directions below.
- **[SRV←tracks]** appears in ALGORITHMS (main.rs:6922) and as **[←tracks]** in the SRV contextual row (main.rs:6680) — same handler, two contexts. Fine.
- Quick color combo (PRODUCTS) vs full manager (Settings) — by design from the last spec, linked by [Edit…]. Fine.
- `self.status` renders in the sidebar empty state, the status bar's fixed slot, AND center-of-map when no texture. Triple-display of one string; benign, but the status bar should be the single home long-term.
- "Show on radar map" exists in both Model and Satellite windows — consistent pattern; the problem is discoverability (1.4) not redundancy.

---

## 2. PEER ANALYSIS

The owner's community are **GR2Analyst natives**; RadarScope is the other shared reference. What matters isn't copying either — it's knowing which muscle memory exists so the refresh spends novelty where it pays.

### 2.1 GR2Analyst (GRLevelX family)

- **One map window, top menu bar + icon toolbar.** Products are toolbar buttons/dropdowns; site switching by typing an ID into a toolbar box; elevation via toolbar arrows or **Up/Down keys**; loop frames via **Left/Right keys** and toolbar transport. *Note the conflict: BowEcho's Left/Right currently steps products, not frames (see §5).*
- **Everything deep is a non-modal dialog**: GIS/placefile manager, color table settings (file-based `.pal` — BowEcho already speaks this dialect, a real asset), archived data lists, polygon warning text via **click-on-polygon popup** (not a sidebar tab — this matters for 1.3.8).
- **Multi-window by OS, not by docking**: GR2A users on multi-monitor desks run the main window big and park dialogs (and GR3/GR2A second instances) on other screens. They have *zero* expectation of drag-dock tabbing, and *full* expectation that panels can become OS windows.
- **No workspaces** — users save window placements and that's it.
- Density norms: GR2A tolerates (celebrates) dense rows of small controls. BowEcho's current sidebar density is *within* community norms; the problem is organization, not density.

**Implication:** map-dominant + toolbar + dialogs + keyboard tilt/frame stepping is the comfort baseline. A unified "layers" concept is *additive* (GR2A has no equivalent — its GIS/placefile manager is the closest), so it must be self-evident, not a relearning exercise.

### 2.2 RadarScope

- **Map IS the app.** Compact pickers (radar / product / tilt) as popovers anchored to a minimal toolbar; warnings are **tap-the-polygon → detail sheet**; layers (lightning, watches, local storm reports) toggle in a single settings sheet with uniform rows.
- **One place to toggle any data type** — the layer sheet — is the closest existing peer to direction A below, and spotters in trucks already know it.
- Desktop RadarScope keeps the same model with an inspector pane; it does not do docking either.

**Implication:** for the single-screen-truck persona, RadarScope proves that *uniform layer rows + polygon-tap detail + map-anchored pickers* work under stress. Both peers agree on one thing: **warning text belongs on the map, at the polygon, not in a tab.**

### 2.3 Honorable mentions

- **AWIPS/CAVE D2D**: procedures/bundles = saved pane+product+layer arrangements — the precedent for direction B. Powerful, and famously a training burden.
- **Supercell-Wx** (Qt): conventional dock widgets — precedent for direction C, and evidence the community can use docks when offered, on desktop-class screens.

---

## 3. THREE CANDIDATE DIRECTIONS

### Direction A — "Everything is a layer"

One unified LAYERS surface where the primary radar, overlay radars, model fields, GOES, placefiles, and warnings are **peers with one row grammar**:

```
[vis] [name/badge]      [status] [opacity────] [row-specific] [⚙] [✕]
```

Windows remain, but demoted to one job: **acquisition & deep browsing** (pick a model run, configure a GOES follow, read a full sounding). Everything you touch *while scanning the map* lives in a row.

```
┌─ SIDEBAR ────────────────────────────────┐
│ RADAR │ LAYERS │ ARCHIVE │ WARN │ ⚙      │   ← 5th tab is an icon; see tradeoffs
├──────────────────────────────────────────┤
│ ▼ LAYERS (6)                             │
│  ◉ KEAX · REF 0.5°      live ●  ▓▓▓▓▓░   │   ← PRIMARY radar: no ✕, badge "◉",
│      (primary — site/products in RADAR)  │     opacity = today's "Radar" slider
│  ☑ KTWX                 live ●  ▓▓▓░░░  [Go][Ref][Pri][✕]
│  ☑ HRRR REFC  f02 ◀ ▶   12z ●   ▓▓▓▓░░  [⚙][✕]   ← ⚙ opens Model window
│  ☑ GOES-19 C13 CONUS    02:46Z● ▓▓░░░░  [⚙][✕]   ← ⚙ opens Satellite window
│  ☑ Warnings  TOR SVR FFW …      fill ▓░ [⚙]      ← ⚙ jumps to WARN tab/filters
│  ☑ "Mesoscale Discussion" (placefile)    [↻][✕]
│                                          │
│  [+ Add layer ▾]                         │
│     Radar overlay ▸ (site picker)        │
│     Model field…   → opens Model window  │
│     Satellite…     → opens Sat window    │
│     Placefile URL… → inline input        │
└──────────────────────────────────────────┘
```

- `[+ Add layer ▾]` is the **single front door** for every map data type — the discoverability fix for "Show on radar map" (the windows keep their buttons, but you no longer need to know them).
- Model master switch, "Keep runs" → Settings ▸ Model. Freshness/[Fetch latest] → the model row's hover + ⚙ window.
- Warning polygon click → **map-anchored detail card** (GR2A/RadarScope behavior) with a "more…" link to the WARN tab. The tab keeps filters/summary.

**Pros:** fixes 1.3.1–1.3.5 and 1.3.8 outright; one mental model that *scales* (lightning, LSRs, surface obs land as rows, not new folds); pure egui, no new dependencies; matches RadarScope's layer-sheet instinct; sidebar stays GR2A-dense.
**Cons:** 5 tabs at 300px min-width = 60px/tab — needs the ⚙ icon tab (or merging WARN into LAYERS later); row grammar must handle asymmetric needs (model row wants hour stepping; warnings row wants fill not opacity) without degenerating back into special cases; doesn't itself answer multi-monitor.

### Direction B — "Workspaces"

Named presets that switch *everything at once*: pane layout, sidebar tab, which windows are open, which layers are visible.

```
TOP BAR:  BowEcho │ Reset · Reload │ ⟨WARNING OPS⟩⟨ANALYSIS⟩⟨SAT/MODEL⟩⟨ARCHIVE⟩ │ Sat Model Guide
            WARNING OPS: 1-pane REF + warnings layer + Warnings tab + all windows closed
            ANALYSIS:    4-pane REF/VEL/CC/ZDR + Radar tab + Sounding window open
            SAT/MODEL:   1-pane + GOES & HRRR layers + Model window open
            ARCHIVE:     1-pane + Archive tab + SPC reports fetched
```

**Pros:** spectacular for the truck persona (one key = warning-day cockpit); AWIPS-procedure precedent; zero restructuring of existing panels — it's a state-snapshot layer on top.
**Cons:** **it preserves every inconsistency in §1.3** — the same junk-drawer fold, just shown/hidden on schedule; state-scoping is genuinely hard (is the color table per-workspace? the site? the loop?); preset-vs-manual divergence ("I moved a window, is that saved?") is a support burden; AWIPS shows the training cost. As the *primary* direction it polishes the mess instead of fixing it.

### Direction C — "Dock system"

Adopt `egui_dock` (or `egui_tiles`): the four floating windows and the sidebar become dockable/tabbable panes; users drag a Sounding tab beside the map, tear Satellite off to monitor 2.

```
┌──────────────────────────────────────┬──────────┐
│ MAP                    │ SOUNDING    │ RADAR ⫝  │
│                        │ (docked     │ LAYERS ⫝ │
│                        │  right)     │          │
├────────────────────────┴─────────────┤          │
│ MODEL DATA (docked bottom, tabbed    │          │
│  with SATELLITE)                     │          │
└──────────────────────────────────────┴──────────┘
```

**Pros:** the multi-monitor desk persona gets exactly what they want; tabbed bottom dock solves window-pileup; layout serialization comes with the crate.
**Cons:** **new dependency tracking egui's release cadence** — eframe is at 0.34.3 and BowEcho upgrades eagerly; egui_dock historically lags releases by weeks, and this repo has already been burned by version-skew CI (fmt/clippy). Drag-docking in a bouncing truck on a trackpad is hostile. GR2A natives have no dock muscle memory (they have OS-window memory — which egui can satisfy *without* a dock crate via native viewports). And like B, it rearranges the mess without fixing the IA. Highest machinery, lowest IA payoff.

### ★ RECOMMENDATION: Direction A, plus native viewport tear-off ("A+")

**Adopt A as the information architecture.** It is the only direction that actually fixes §1.3, it requires zero new dependencies, and it matches both peer instincts (RadarScope's layer sheet; GR2A's click-the-polygon warnings).

**Answer multi-monitor with egui native viewports, not egui_dock.** eframe 0.34 supports deferred viewports (`ctx.show_viewport_deferred`) — real OS windows sharing the egui Context (textures included). Give each data window a "Detach ⇱" titlebar button that re-hosts its `ui()` in a viewport. That *is* the GR2A multi-monitor model (park windows on other screens), with none of the dock-crate risk. On a single laptop screen, nothing changes — the windows stay floating as today.

**Take B's cheapest 20% later:** after A lands, a "save/restore session layout" (open windows + tab + pane grid + layer set, in `config.json`) gets most of the truck win without preset semantics. Full named workspaces stay post-v1. C (docking) is explicitly deferred post-v1; revisit only if viewport tear-off proves insufficient.

Grounding in this user base: spotters on single screens get a shorter Radar tab, uniform layer rows, polygon-tap warning text, and tool hotkeys — all stress-compatible. GR2A desk users get tear-off OS windows and keep every existing keybinding. Nobody is asked to learn docking, and nobody's number-row reflexes break.

Target end-state tree (deltas only):

```
TOP BAR:  BowEcho │ Reset View · Reload │              │ Sounding · Sat · Model · Guide
                    (actions, left)        (spacer)      (window toggles, right-aligned)
SIDEBAR:  RADAR · LAYERS · ARCHIVE · WARN · ⚙
  RADAR   = Panes/SITE/PRODUCTS/TILT/LOOP/ALGORITHMS/TOOLS  (Layers fold REMOVED → shorter)
  LAYERS  = unified rows + [+ Add layer ▾]                  (new tab, content from the fold)
  ARCHIVE = unchanged (rename "Frames" → "Fetch N scans")
  WARN    = unchanged content; polygon click now ALSO pops map-anchored detail card
  ⚙       = Display · Color tables · Hotkeys · Performance · NEW: Model (master switch,
            Keep runs, store path readout) · NEW: Satellite (retention/disk usage)
WINDOWS:  Model data (gains "Download" section, killing the separate Model download window)
          Satellite (GOES) · Sounding (gains top-bar reopen toggle) — all gain [Detach ⇱]
```

---## 4. MIGRATION PLAN

Ordered, lowest-risk first; **every step compiles, ships, and improves the app alone**. Machinery column: *reorg* = moving existing draw calls/state, *state* = new fields/persistence, *egui+* = new egui machinery (still no new crates until step 9 — and step 9 is stdlib-egui, not a dependency).

| # | Step | Machinery | Notes |
|---|---|---|---|
| 1 | **Top-bar regroup + Guide + Sounding toggle.** Left: actions; right-aligned cluster: `Sounding · Sat · Model · Guide` as selectable toggles. Sounding toggle = `native_skewt_open = !native_skewt_open` (enabled iff `native_sounding.is_some()`) — fixes 1.3.10. Guide opens the cheat-sheet (see §5.4). | reorg | `top_bar` main.rs:5899. ~1 hour. |
| 2 | **Fix the model master-switch fight (1.3.7).** Clicking top-bar [Model] while `!model_enabled` either flips `model_enabled = true` (recommended — the button states intent) or disables the button with a tooltip. One-frame-flash bug dies. | reorg | `top_bar` + `poll_model_layer` main.rs:9577. |
| 3 | **`layer_row()` helper + honest count.** One fn drawing the §3-A row grammar; port the four existing row types (overlay radar, GOES, model, placefile) onto it *inside the existing fold*; add the primary-radar row (absorbing the "Radar" opacity slider); `layer_count` counts everything it shows. No moves yet — pure de-special-casing. | reorg | main.rs:6276–6524 + `radar_layers_panel` 7150. The keystone refactor; everything later reuses `layer_row()`. |
| 4 | **Evict settings from the fold.** "Keep runs" + master switch → Settings ▸ Model (new fold in `settings_panel`, main.rs:6056). Freshness line + [Fetch latest]/[Download…] collapse into the model layer row (hover = freshness; ⚙ = window). Fold shrinks to: rows + placefile add + [+ Add layer ▾] stub. | reorg | |
| 5 | **`[+ Add layer ▾]` menu** (radar overlay ▸ site list, model field → opens Model window, satellite → opens Sat window, placefile → inline URL row). The single front door (§1.4). | reorg + state | One menu_button; reuses existing open-window/add paths. |
| 6 | **Promote LAYERS to a tab.** Move the (now-clean) fold body to `SidebarTab::Layers`; Radar tab loses the fold (its biggest scroll cost). Settings tab label becomes ⚙ icon to keep 5 tabs ≥60px at min width. Keep a one-line "Layers: 6 (2 hidden)" link-row in RADAR▸SITE for context. | reorg | `SidebarTab` main.rs:2233, `SIDEBAR_TABS`, `side_panel` match 5931. Same pattern as the Archive-tab addition. |
| 7 | **Map-anchored warning card (1.3.8).** Polygon click → small anchored card (event · expiry · max hail/wind tags · [Full text → WARN tab]). Reuses `hazard_record_detail_lines`. Honors the last spec's deferred item 10. | reorg + state | `egui::Area` anchored at click; no new deps. Ship behind a Settings checkbox if nervous. |
| 8 | **Merge Model download INTO Model data window.** Host `download_panel.ui()` (already event-driven in main.rs:9113–9145) inside the Model window as a `▸ Download` section / second tab. One window fewer; the [Download…] row button just opens Model with that section expanded. rw_ui needs **no upstream change** — BowEcho composes the panels it already owns references to. | reorg | Kills a whole window class. |
| 9 | **Detach ⇱ (viewport tear-off)** for Sounding first (zero worker coupling — it draws from an `Arc<NativeSounding>`), then Satellite, then Model. `ctx.show_viewport_deferred` with the same body fn; persisted `detached: bool` per window. Risks to verify on Windows+macOS: texture sharing across viewports (egui TextureManager is per-Context — shared, should be fine) and repaint wakeups from worker threads targeting the right viewport. | **egui+** | eframe 0.34 built-in; **no egui_dock**. This is the only step needing platform QA. |
| 10 | **Session-layout save/restore** (open windows, detached flags, sidebar tab, pane grid, layer list) in `config.json` on exit / restore on launch. The B-lite payoff. Named workspaces: post-v1. | state | Builds on `AppSettings` (settings/src/lib.rs). |

Steps 1–8 are a normal sidebar-spec-style session each or smaller; the whole arc is incremental with no flag-day. If the refresh gets cut short after any step, the app is still strictly better than today.

**Out of scope, deliberately:** rewriting rw_ui panel internals (external pinned dep — `crates/app_ui/Cargo.toml` rev `e749c2a`); docking (post-v1, only if tear-off disappoints); touch targets / mobile.

---

## 5. KEYBOARD-FIRST NOTES

### 5.1 What exists (preserve byte-for-byte)

| Binding | Action | Where |
|---|---|---|
| `1–9, 0` | Products (config.json remappable; default REF/VEL/SRV/RHO/ZDR/SW/CREF/ET/VIL/VILD) | `handle_product_hotkeys` main.rs:3485; defaults settings/src/lib.rs:57 |
| `←/→` | Step product (focused pane) | `handle_keyboard_navigation` main.rs:3433 |
| `↑/↓` | Step tilt (focused pane) | same |
| `Shift+click` map | Pin/release inspector card | main.rs:7947 |
| `Alt+click` map | Model sounding · `Ctrl+Alt` = follow-mouse | main.rs:8023 |
| Right-click map | Best-radar menu / clear armed tool | main.rs:7906 |
| Click pane | Focus pane (sidebar+keys edit it) | main.rs:8127 |

All routing goes through `text_edit_focused()` and `consume_key` guards — keep that pattern for every addition. The hotkey-prefixed product grid ("1·REF") is the discoverability win of the last redesign; extend the same prefixing to any control that gains a key.

### 5.2 The GR2A arrow conflict — handle it honestly

GR2A muscle memory: `←/→` = **loop frames**, `↑/↓` = tilt. BowEcho shipped `←/→` = products. Do **not** silently flip it (that betrays BowEcho's own early adopters). Instead:

- Add a `key_profile` setting: `"bowecho"` (default, today's behavior) | `"gr2a"` (`←/→` frames, `Shift+←/→` products).
- Surface the choice once in the Guide ("Coming from GR2Analyst?").

### 5.3 New bindings (proposed defaults — all remappable via the same config map)

| Key | Action | Rationale |
|---|---|---|
| `Space` | Play/pause loop | Universal transport; currently mouse-only |
| `,` / `.` | Frame back / forward | Works in both key profiles; near-universal in editors |
| `Esc` | Disarm tool → close warning card → close topmost window (in that order) | One panic button; complements right-click-clears |
| `X` | Arm/disarm cross-section | fixes 1.3.11 (bottom-of-scroll arming) |
| `R` | Arm/disarm Vrot tool | "rotation"; `V` reads as velocity-product |
| `I` | Toggle inspector card | |
| `W` | Warnings tab (toggle back to Radar) | warning-ops speed |
| `L` | Layers tab (toggle back to Radar) | pairs with the new tab |
| `Ctrl+1..4` | Focus pane 1–4 (`Ctrl+1` = main) | number row is taken by products; pane focus is currently click-only |
| `F1` | Guide | convention |

Implementation: generalize `product_hotkeys` into a `key_bindings: BTreeMap<String, String>` table in `AppSettings` (action-id → key), with the existing product map untouched for back-compat. Settings ▸ Hotkeys then renders **the whole registry** (today it lists only the number row — 1.4), and the Guide cheat-sheet is generated from the same table, so docs can never drift from bindings.

### 5.4 Keyboard rules for the refresh (binding contract)

1. **No step in §4 removes or rebinds an existing key.** Steps 1–8 are keyboard-neutral; §5.3 ships as its own step (slot it anywhere after step 1).
2. Every new interactive surface must answer "what's its key?" or explicitly opt out in the spec — the layer rows get none in v1 (mouse domain), the tabs/tools/transport do.
3. Detached viewports (step 9) must keep receiving the global keys: route `handle_keyboard_navigation` from raw input on the main viewport only, and verify focus behavior when a tear-off window is foreground (known egui multi-viewport sharp edge — QA item for step 9).
4. Tooltips name their key, grid-style ("hotkey 3"), everywhere a binding exists.

---

## Appendix: code anchors for the implementing session

| Surface | Anchor | main.rs @ 0781794 |
|---|---|---|
| Root layout (panels + windows) | `impl eframe::App for ViewerApp::ui` | 5793–5895 |
| Top bar | `top_bar` | 5899 |
| Sidebar shell + tabs | `side_panel`, `SidebarTab`, `sidebar_tab_bar` | 5926, 2233, 6096 |
| Radar tab | `radar_controls_panel` | 6130 |
| Layers fold body | inline in radar_controls_panel | 6274–6524 |
| Overlay radar rows | `radar_layers_panel` | 7150 |
| Archive tab | `archive_panel` | 5223 |
| Warnings tab | `hazard_panel` | 7397 |
| Settings tab | `settings_panel` | 6056 |
| Loop transport | `frame_history_panel` | 6991 |
| Model window host | inline in `ui` / `ModelDataDock::ui` | 5857–5874 / model_data.rs:231 |
| Model download window | `model_download_window` | 9085 |
| Satellite window | `satellite_window` | 9385 |
| Sounding window | inline in `ui` / `poll_native_sounding` | 5879–5894 / 9011 |
| Status bar | `status_bar` | 7802 |
| Map interactions | `single_pane_canvas`, `grid_canvas`, `best_radar_context_menu` | 7860, 8105, 8829 |
| Keyboard | `handle_keyboard_navigation`, `handle_product_hotkeys` | 3433, 3485 |
| Hotkey defaults | `default_product_hotkeys` | crates/settings/src/lib.rs:57 |
| Sidebar width constants | `SIDEBAR_*_WIDTH` | 99–103 |

Coordination note (same as the last spec's gate): a separate UI agent has owned app_ui work on the `rra-review` clone (`perf/engine-fast-path`); confirm no concurrent app_ui session before starting, branch off the then-current mainline (e.g. `ui/refresh-v1`), and do not touch dealias or render-worker logic from this branch.
