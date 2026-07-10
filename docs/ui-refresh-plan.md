# BowEcho sidebar UI refresh — design spec (binding for all waves)

Owner verdict (2026-07-10): "every menu needs to be redone... best UI possible for this
scientific oriented gr2aish awips ish app." Target feel: GR2Analyst/AWIPS — narrow,
dense, ruthlessly aligned, zero wasted width.

## The one hard rule
EVERY row must render correctly at 320 pt panel width: wrap, truncate-with-hover, or
fold into a popover. Nothing may clip and nothing may force the user to widen the
panel. 380 pt (the default) must feel comfortable, not survivable.

## Code anchors (from recon 2026-07-10)
- Panel: egui::Panel::right("product_tilt_panel"), main.rs:18769; widths in
  ui_theme.rs:43 (default 380, min 300, max 900). Width NOT persisted today.
- Tabs: SidebarTab main.rs:7170 (Radar / Layers="Map" / Severe="Alerts" / Data /
  Settings); dispatch fn side_panel main.rs:20806.
- Tab bodies: radar_controls_panel main.rs:21998 (ALL-CAPS section_header, NOT
  collapsible); customization_panel settings_ui.rs:11 + layers_rail.rs;
  hazard_panel hazard_ui.rs:9; data_panel settings_ui.rs:582; settings_panel
  settings_ui.rs:1769 (these use persisted remembered_section, main.rs:20881).
- Loop bar: frame_history_panel main.rs:23140, rendered on Radar (22091) AND Data
  (settings_ui.rs:585). Its mega-row main.rs:23154-23283 is the #1 width offender.
- Collapse persistence: app_settings.sidebar_section_open (settings/src/lib.rs:653)
  via section_open/set_section_open (main.rs:20863/20871).
- Existing helpers: section_header 20967, section_rule 20979, remembered_section
  20881, fixed_action_button 37761, fixed_height_scroll 37733, wrapped_label 37757,
  live_chunk_readout_row 41687; constants in ui_theme.rs.

## The panel kit (new module crates/app_ui/src/panel_kit.rs)
All tabs build from these primitives ONLY. No hand-rolled ui.horizontal rows for
label+control pairs.
1. kit section(ui, key, title, default_open, body) — persisted collapsible section:
   thin rule + uppercase 12.5pt strong title in SUBHEAD_COLOR + chevron; state via
   the existing sidebar_section_open map. Replaces BOTH section_header (Radar) and
   remembered_section styling so every tab looks identical. remembered_section
   becomes a thin wrapper over it (keys unchanged — user state survives).
2. kit row(ui, label, control_fn) — label left, control right-aligned. Label column
   = 44% of available width, clamped [110, 170] pt; control gets the rest. Labels
   truncate with hover. No floating checkboxes at arbitrary x.
3. kit slider_row(ui, label, value, range, display) — label col + slider track
   filling the middle + value right-aligned in a fixed 48pt monospace slot
   (Slider::show_value(false), value drawn by the kit). Track never drops below
   70pt (at panel minimum).
4. kit chip_grid(ui, chips) — wrapping grid of fixed-width selectable chips
   (min 52pt, grow to fill the row evenly); chip = optional small weak hotkey
   prefix + label, truncating. For PRODUCTS, outlook buttons, family filters.
5. kit status_block(ui, primary, secondary) — up-to-2-line small monospace weak
   text, each line truncating with full-text hover. For radar/chunk status lines.
6. kit about(ui, key, text) — collapsed-by-default "About..." disclosure for
   explainer prose (velocity units note, SmartScreen paragraphs). Prose never
   renders inline-expanded by default.
7. kit gear_popover(ui, id, body) — small gear button opening a popover for
   advanced/rare controls (loop bar power knobs, per-layer settings where already
   gear-based keep their pattern).

## Global changes (wave 1)
- Persist sidebar width: new app_settings field (Option<f32>), clamped 300..900,
  applied at panel build, saved via the existing debounced settings persistence.
- Loop bar (both tabs): row 1 = transport only: step-back, Play/Pause, step-fwd,
  "frame i/N", speed combo. Row 2 = the status_block. EVERYTHING else (frame-limit
  combo, N DragValue, RAM budget combo + warning, in-scan sweeps checkbox + filter
  combo, fps, loop multiplier, Free/720/Auto) moves into kit gear_popover "Loop
  settings", organized with kit rows. Zero behavior/state changes.
- TILT rows: left-aligned, truncating, monospace columns (#NN angle radials time);
  fits at 320.
- Radar tab sections become kit sections (new keys: radar_loop, radar_products,
  radar_tilt, radar_site, radar_algorithms, radar_tools; all default OPEN).
- PRODUCTS buttons -> chip_grid; hotkey digit as weak prefix; derived rows same grid.
- Settings tab: Display sliders -> slider_row; velocity-units note and the
  Defender/SmartScreen paragraphs -> kit about; rows normalized to kit row.
- Alerts list rows (wave 2 but rule set now): left-aligned tabular button content
  (type chip | id | office | expires), truncating, never centered.
- Capitalization: kit section renders titles uppercase itself; callers pass
  "Loop"/"Products"/... Buttons stay sentence case.

## Waves
1. panel_kit.rs + width persistence + loop bar + Radar tab + Settings tab. RC.
2. Map (layers_rail rows to kit grammar), Alerts (tabular rows, wrapped filters),
   Data (sections to kit; pack rows compact). Adjust per owner RC feedback.
3. Polish pass from RC feedback; delete dead helpers once no callers remain.

## Owner mandate upgrade (2026-07-10): "we could have a totally new ui"
The owner has delegated design authority for a full visual refresh, not just row
reorganization. Additional scope on top of the waves above (sequenced AFTER wave 1
lands so everything builds on the kit):
4. THEME PASS (ui_theme.rs is the seam): a refined dark scientific palette —
   deliberate neutral ramp (background/raised/inset), one accent used sparingly
   (selection + live indicators), status colors (live green, warn amber, alert
   red) used ONLY for meaning, never decoration; a typographic scale (11/12.5/14
   with monospace for all numerics/ids/times); consistent 4pt spacing rhythm;
   restrained rounding (2-3px) and hairline separators over heavy rules. Applied
   via egui Style + the kit, so it lands everywhere at once.
5. TOP BAR: AWIPS-style information strip — site, product, VCP, scan time/age,
   loop state as compact readouts (monospace), window controls untouched.
   STATUS BAR: same treatment, truncating message region.
6. COMMAND PALETTE (stretch, own branch): Ctrl+K fuzzy launcher over the
   action/hotkey registry so the hundred features stop needing menu real estate.
7. Iteration protocol: the integrator builds, screenshots at 320/380/650 and in
   both a radar-loaded and empty state, READS the images, and iterates with the
   implementing agent BEFORE the owner RC. The owner sees curated candidates,
   not first drafts. Visual judgment calls the integrator cannot settle from
   screenshots go to the owner as small A/B choices, not open questions.

## Non-negotiables
- Zero functional changes: every control keeps its behavior, hotkey, and state.
- Existing sidebar_section_open keys keep working (no user-state loss).
- No decorative emojis in code/UI text beyond existing literal button labels.
- Gates green (fmt/clippy/test) on the nodes for every commit; baseline 2053.
- Headless nodes cannot judge looks: the integrator builds the exe, screenshots
  the panel at 320/380/650 widths, and READS the screenshots before calling
  anything done. Owner RC is final acceptance.
