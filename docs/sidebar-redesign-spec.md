# BowEcho Sidebar Redesign — SYNTHESIZED SPEC (v1, judge output)

Base skeleton: Design B (3 tabs, GR2A order). Grafts: A's stable hot block + hotkey prefixes + verified enum facts; C's same-row add_enabled conditionals, Layers fold above the volume gate, one-line status, id_salt pinning. All line numbers = C:\Users\drew\radar-work\radar-rs-analyst\crates\app_ui\src\main.rs (15,939 lines). Every widget in the inventory has a destination; nothing is removed. No egui::Window — everything stays in the existing right panel (shell 4970–4974 untouched).

## TAB BAR — 3 tabs: RADAR · WARNINGS · SETTINGS
- Delete `ui.heading("Controls")` (5007). Tab bar becomes row 1.
- `enum SidebarTab { Radar, Warnings, Settings }` (2029). sidebar_tab is runtime-only (field 453; constructed at 2187 and 15424; never serialized — VERIFIED, no migration needed). Default Radar.
- SIDEBAR_TABS (2036) → 3 entries; tooltips (2043): Radar "Site, products, tilt, loop, algorithms — live operations"; Warnings "Warnings, watches, MDs, and alert filters"; Settings "Basemap, color tables, hotkeys, diagnostics — always available".
- side_panel match (5012–5049): Warnings → hazard_panel (keep ScrollArea id "sidebar_hazards_tab" so scroll state survives), Settings → new fn settings_panel (new ScrollArea id "sidebar_settings_tab").
- Tab width: replace hardcoded 67.0 (5059) with `(ui.available_width() - 2.0 * ui.spacing().item_spacing.x) / 3.0`.
- New helper `fn section_header(ui, label)` = add_space(8) + separator() + small strong uppercase label (Design B).
- New field: `open_color_tables_request: bool` (one-shot, not persisted).

## TAB 1: RADAR (radar_controls_panel rebuilt; top-to-bottom)

HOISTED FIRST: move the editing_pane/editing_product/editing_cut computation (5163–5174) to the top of the fn — it does NOT borrow self.volume (verified), only grid_layout/active_pane/extra_panes.

R0. PANES row (always visible): label "Panes" + selectable "1"/"2"/"4" (move 5071–5099, handlers/persistence unchanged). Directly below, the conditional colored "Editing pane N — click the main (top-left) pane to edit all" notice (move 5220–5228) — pane context sits above everything it affects (Design C's A2).

R1. section_header "SITE" (always visible):
  1. Row: ComboBox site_combo (5109–5119; widen from 220 px to fill) + Button "Center" (from 5121–5141).
  2. Row: Button "Load Latest" (rename "Load Selected") · Button "Load Loop" · Checkbox "Live" · Checkbox "Chunks" (rest of 5121–5141, same handlers; Chunks deliberately stays here — it is a live-ops toggle, rejecting C's move to Settings).
  3. STATUS LINE, exactly one line, always rendered: if volume → compact weak label "{site} · VCP {vcp} · {HH:MM:SS}Z · {N} cuts" with `.on_hover_text` carrying the full former block (5202–5214 text). If no volume → `ui.label(&self.status)` (preserves the old gate's else label, 5147).
  4. Conditional weak line: live-chunk readout (5209–5211 logic) — only during live chunk assembly, exactly when it matters (addresses B's risk 5).
  5. CollapsingHeader "Volume details" (default CLOSED, volume only): verbatim full label block 5202–5214 incl. Status line and "{N} cuts, {M} radials".

R2. CollapsingHeader "Layers (N)" — ABOVE the volume gate, `id_salt("layers_fold")` to pin open-state against the changing count (C's catch); N = overlay count + enabled placefiles; default open iff N>0:
  1. radar_layers_panel body (5840–5956): "Clear" button, per-overlay rows (visibility checkbox · site label w/ details tooltip · state dot · opacity slider · Go/Ref/Pri · ✕). BUG FIX: ✕ tooltip (5924) → literal "Remove overlay".
  2. separator, then the Placefiles block verbatim (former CollapsingHeader body 5397–5453): URL TextEdit + "Add"; per-slot enable/title/↻/✕ rows. Grouping rationale (C): both are "things drawn on the map from elsewhere", and placefiles no longer vanish when volume == None.

— VOLUME GATE — keep `let Some(volume) = &self.volume else { return; };` (5146–5149) but the else body becomes a bare `return` (status already shown in R1.3). Everything below genuinely needs volume; everything global has moved above it or to Settings. This is the lowest-risk gate change: no if-let nesting, no borrow restructure.

R3. section_header "PRODUCTS":
  1. Product grid: horizontal_wrapped selectable buttons (5229–5250, handlers unchanged) with TWO additions (A+B): label prefixed with its bound number-row key from app_settings.product_hotkeys ("1·REF", "2·VEL"…; unbound products unprefixed), tooltip = full product name + key. This makes the existing hotkeys (default_product_hotkeys, crates\settings\src\lib.rs:48–64) discoverable on stream.
  2. CONTEXTUAL ROWS directly under the grid — each exactly ONE row, ALL gated on editing_product (fixes the main-vs-focused split at 5295/5592/5811), max one family block active at a time so the tilt list below shifts by at most one row:
     a. Velocity family (rework 5295–5349, C's same-row treatment): ☐ "Unfold VEL" FIRST, then ComboBox dealias_engine ("Region"/"Cascade (beta)") on the SAME row wrapped in `ui.add_enabled_ui(self.unfold_velocity_display, …)` — kills the pops-in-above-its-checkbox bug with zero vertical reflow — then ☐ "Flip VEL colors" at row end. Unfold/engine only when base VEL moment (existing matches! at 5296). Tooltips gain "(applies to all panes showing velocity)" — gating follows the focused pane, state stays global (B risk 1 / C risk 3: do NOT split the state).
     b. SRV (move 5592–5616, gate → editing_product): label "Motion" + dir DragValue (0–359°) + spd DragValue (0–120 kt) + conditional small "←tracks" button (same handler as 5380) when a track-derived mean motion exists.
     c. MEHS (move 5558–5590, one row): label "Hail 0°C/−20°C (km)" + the two DragValues, clamping unchanged. Keep the Witt et al. 1998 citation comment; likewise preserve Stumpf et al. 1998 / Mitchell et al. 1998 (rotation markers) and Johnson et al. 1998 SCIT (storm tracks) citations through every move.
  3. COLOR row: label "Color" + active_product_color_picker — fn signature changes to take the product (or family) as a parameter instead of reading self.selected_product at 5811 (fixes the main-pane bug); keep the summary label (5837); add small Button "Edit…" → sets self.sidebar_tab = Settings, self.color_table_target = family (reusing the Colors-tab "Current" jump logic), self.open_color_tables_request = true.
  4. HIDE-BELOW row (move 5253–5293): checkbox "Hide below"/"Hide |val| below" + DragValue on ONE row, DragValue wrapped in add_enabled(checked) instead of conditional render (C) — no pop-in, no reflow. Already gated on editing_product (correct today).

R4. section_header "TILT": row with label "Tilt" + weak "↑/↓" hint + conditional "Follow main tilt" button / "following main" weak text (5621–5634); then tilt_list ScrollArea verbatim (5636–5681, 168 px max, existing id). Stable position: only ever one contextual row above it.

R5. section_header "LOOP" (frame_history_panel 5684–5808 restructured in place; call site stays):
  1. Row: transport "<" / "Play⇄Pause" / ">" (5719–5760) + weak "{idx+1}/{n}" + right-aligned ComboBox history_frame_limit "N frames" (from 5689 area; HISTORY_SIZE_OPTIONS line 63).
  2. Full-width scrub Slider (5762–5775).
  3. ONE truncated label: selected-frame status (replaces fixed scroll 5707–5715; full text in hover tooltip — information preserved).
  4. CollapsingHeader "Frames (N)" `id_salt("loop_frames")`, default CLOSED: the wrapped 72-px frame buttons scroll (5777–5799).
  5. Empty state (<2 frames): rows 1–4 replaced by weak "No loop — use Load Loop" (keeps the section header in place for position stability; rejects C's zero-pixel collapse).

R6. section_header "ALGORITHMS" (GR2A convention): ☐ "Rotation markers" (5352–5364); row ☐ "Storm tracks" + conditional small "SRV←tracks" (5367–5394, unchanged).

R7. section_header "TOOLS": ☐ "Inspector card" (5529–5532); row ☐ "Cross-section" + Button "Clear XS" (5533–5556; bottom cross_section_panel at 8286 untouched; Shift+click pin note stays in tooltip).

REMOVED from Radar tab (all relocated): Smooth display (5481–5489), Basemap + Bold town labels (5492–5527), Hotkeys header (5456–5478) → Settings; Placefiles (5397–5453) → R2; verbose volume labels → R1.3/R1.5; old color-picker call site (5251) → R3.3.

## TAB 2: WARNINGS (hazard_panel 6076–6164, light touch — kept whole per B; A's split of summary/local-file to Settings rejected as fragmenting one workflow)
  W1. Drop `ui.label("Hazards")` (6077).
  W2. Row: ☐ "Show" · ☐ "Active only" · ☐ "Auto-refresh" (renames of Show/Active/Auto).
  W3. horizontal_wrapped family filters TOR/SVR/FFW/Flood/SMW/SQW/Watch/MD/SPS — unchanged (HAZARD_FILTER_FAMILIES 99–109).
  W4. Slider "Fill" 0–80 — unchanged.
  W5. Row: Button "Refresh Live" + Button "Clear" — unchanged.
  W6. Conditional selected-hazard detail scroll (6123–6136) — unchanged, always visible when a polygon is selected (rejecting C's collapsed-fold regression).
  W7. Summary scroll (6138–6148) — unchanged.
  W8. CollapsingHeader "Load from file" (rename of "Local file", 6150–6163) — unchanged.

## TAB 3: SETTINGS (new fn settings_panel; ALL volume-independent; fixes the gate-hides-global-prefs bug)
  S1. CollapsingHeader "Display" (default OPEN): ☐ "Smooth display" (5481–5489); row "Basemap" + ComboBox basemap_style (5492–5516); ☐ "Bold town labels" (5517–5527).
  S2. CollapsingHeader "Color tables" (default CLOSED; force-opened by open_color_tables_request via `egui::collapsing_header::CollapsingState::load_with_default_open(ctx, id, false)` + `set_open(true)` + store, then CLEAR the flag same frame — one-shot, never pass Some(true) unconditionally): entire color_table_panel body (5958–6023) minus its `ui.label("Colors")` — family target combo + "Current", "{family}: {table}" labels, Built-ins combo, path TextEdit, "Load Table"/"Reset Slot", status scroll. This is the ONLY full color manager; R3.3 is the quick pick; "Edit…" links them.
  S3. CollapsingHeader "Hotkeys" (default CLOSED): former body 5456–5478 verbatim, plus one weak line "←/→ product · ↑/↓ tilt (focused pane)" (A's addition).
  S4. CollapsingHeader "Performance" (default CLOSED): stats_panel body (6166–6188) minus its "Performance" label — ☐ "Details", Render/Worker/Texture/Decode/Load ms labels, overlay/range counts, timing_readout.

## CONDITIONAL VISIBILITY RULES (complete)
- Always (any tab state, no volume): tab bar; R0 panes; R1 site/load/status; R2 Layers fold; entire Warnings tab; entire Settings tab.
- Volume-gated: R1.4 chunk readout (live frames only), R1.5 Volume details, R3–R7.
- editing_product-gated (focused pane): VEL row (velocity family; unfold/engine additionally require base VEL moment; engine combo enabled iff unfold checked), SRV row, MEHS row, color-row family, hide-below family/label.
- Other conditionals: editing-pane notice (multi-pane + non-main focus); "Follow main tilt" vs "following main" (extra pane focused); "SRV←tracks" (track mean motion exists); Loop body vs empty hint (frames>1); selected-hazard detail (polygon selected); Layers fold default-open (N>0).
- Global-state caveat: unfold/flip/storm-motion/color-family slots remain app-global per family; only their VISIBILITY follows editing_product. Tooltips say "(all panes)".

## VISIBLE BUDGET (volume loaded, defaults): panes row + 3 site/status rows + 2 fold titles (Layers, Volume details) + product grid + ≤1 contextual row + color row + hide-below row + tilt header/list + transport + slider + status line + Frames fold title + 2 algorithm rows + 2 tool rows. Worst case (300 px, quad + SRV + unfold) fits without inner scrolling beyond tilt_list; the 7 section headers cost less height than the removed Placefiles/Hotkeys/Basemap/Smooth/verbose-labels blocks.ORDERED IMPLEMENTATION CHECKLIST (one focused session; all edits in crates/app_ui/src/main.rs unless noted)

0. COORDINATION GATE: a separate UI agent owns app_ui on the rra-review clone (branch perf/engine-fast-path). Confirm no concurrent app_ui work before starting; implement here on a new branch off fix/region-based-velocity-dealias (e.g. ui/sidebar-redesign). Do NOT touch dealias logic.

1. Scaffolding (compiles after this step): rename enum SidebarTab variants → Radar/Warnings/Settings (2029–2034); SIDEBAR_TABS → 3 entries (2036–2041); sidebar_tab_tooltip (2043–2050); fix BOTH construction sites 2187 and 15424; side_panel match (5012–5049): Warnings→hazard_panel (keep id "sidebar_hazards_tab"), Settings→stub settings_panel (new id "sidebar_settings_tab"); delete heading (5007); tab width 67.0 (5059) → (available_width − 2·spacing)/3. Grep `SidebarTab::` for stragglers. No serde migration (verified non-persisted).

2. Add `fn section_header(ui, label)` helper and field `open_color_tables_request: bool` (init false at 2187 and 15424).

3. settings_panel: cut-paste Smooth (5481–5489), Basemap (5492–5516), Bold labels (5517–5527) into ▸ Display (open); color_table_panel body (5958–6023, drop "Colors" label) into ▸ Color tables (closed, CollapsingState::load_with_default_open + set_open(true)+store when open_color_tables_request, clear flag same frame); Hotkeys body (5456–5478) + weak arrow-keys line into ▸ Hotkeys (closed); stats_panel body (6166–6188, drop "Performance" label) into ▸ Performance (closed). Compile check: none of these borrow the volume locals (5151–5174) — verified volume-free, but each paste needs a build.

4. Parameterize `active_product_color_picker` (5810–5838): take the editing product (or family) as a param, replace `self.selected_product` at 5811; append "Edit…" button → sidebar_tab=Settings, color_table_target=family, open_color_tables_request=true.

5. radar_controls_panel restructure (the big move; keep handlers byte-identical, move draw calls only):
   a. Hoist editing_pane/editing_product/editing_cut (5163–5174) to top of fn (no volume borrow — verified).
   b. Top: Panes row (5071–5099) + editing-pane notice (5220–5228).
   c. SITE section: site combo widened + "Center"; "Load Latest"/"Load Loop"/Live/Chunks (5100–5141). One-line status (volume one-liner w/ full-block tooltip, else self.status — preserves 5147's else label); conditional live-chunk weak line (5209–5211); ▸ Volume details (closed) = verbatim 5202–5214.
   d. ▸ Layers (N) with id_salt("layers_fold"), default open iff N>0, ABOVE the gate: radar_layers_panel body (5840–5956) + ✕ tooltip fix at 5924 ("Remove overlay") + separator + Placefiles body (5397–5453).
   e. Volume gate (5146–5149): else branch → bare `return;` (status now lives above).
   f. PRODUCTS: grid (5229–5250) + hotkey prefixes/tooltips from app_settings.product_hotkeys; VEL row reworked from 5295–5349 (Unfold first, engine combo same row in add_enabled_ui(unfold), Flip last; gate selected_product→editing_product; "(all panes)" tooltips); SRV row from 5592–5616 (gate→editing_product, add "←tracks" sharing the 5380 handler); MEHS row from 5558–5590 (one row, keep Witt 1998 comment); color row (call step-4 fn with editing_product); hide-below (5253–5293) one row with DragValue add_enabled.
   g. TILT: header + 5618–5681 unchanged.
   h. LOOP: restructure frame_history_panel (5684–5808): transport (5719–5760) + right-aligned frame-limit combo; slider (5762–5775); selected-frame status → one truncated label w/ tooltip (from 5707–5715); ▸ Frames (N) id_salt("loop_frames") closed = 5777–5799; empty hint when frames<2.
   i. ALGORITHMS: 5352–5364, 5367–5394 (preserve Stumpf/Mitchell/Johnson citations). TOOLS: 5529–5556.
   j. Delete now-dead originals (5251 call, 5202–5214 inline block, 5295–5349, 5397–5478, 5481–5527, 5558–5616 originals); compile.

6. Warnings tab: drop label (6077); rename checkboxes "Active only"/"Auto-refresh"; rename "Local file"→"Load from file" (6150). Body otherwise untouched.

7. ID hygiene pass: every dynamic-titled CollapsingHeader ("Layers (N)", "Frames (N)") has a fixed id_salt; no widget renders twice per frame (color_table_panel exists ONLY in Settings; quick combo id "active_product_color_preset" distinct from "color_table_builtin_preset" — verified distinct salts).

8. Build + `cargo clippy -p app_ui` + `cargo fmt` (note: fmt/clippy version skew fails CI on main — match the repo's pinned toolchain, don't reformat unrelated code).

9. Manual regression matrix:
   - Cold start, no volume: site/load/Layers/placefiles/status visible on Radar; Warnings and Settings fully functional (the headline fix).
   - 1/2/4 layouts: focus each pane; verify VEL/SRV/MEHS rows, color combo, hide-below all follow the focused pane; toggle Unfold/Flip from an extra-pane focus and confirm ALL panes showing that family re-render (if clear_texture() only invalidates the main pane, also reset extra_panes[*].render_ms as the product-click handler does at 5237–5239).
   - Engine combo disabled-not-hidden when Unfold off; no vertical reflow on toggle.
   - Load no-op while in flight; Live auto-refresh; Chunks toggle; "SRV←tracks" enable condition; Follow-main vs pinned tilt at new position; Play/Pause/scrub; frame-limit combo; Frames fold open-state survives N changes.
   - "Edit…" jumps to Settings with Color tables open and correct family preselected; header closable afterwards (flag cleared).
   - Hazard polygon select shows detail on Warnings; placefile add/refresh/remove with no volume; overlay ✕ tooltip reads "Remove overlay".
   - Resize panel to 300 px: worst case (quad + SRV + unfold) has no horizontal clipping; per-tab scroll positions persist.

10. OPTIONAL (flagged, behavior change beyond moving draw calls — get user sign-off first): clicking a hazard polygon on the map sets sidebar_tab = Warnings so the detail block is immediately visible (recovers Design A's inline-alert speed without duplicating widgets).