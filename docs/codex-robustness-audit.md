# Codex Robustness Overhaul

Goal: make BowEcho as robust, fluid, and workflow-complete as practical. This is a living audit and handoff note for the `codex/robustness-overhaul` worktree.

## Baseline

- Worktree: `C:\Users\drew\radar-work\wt-codex-robustness`
- Branch: `codex/robustness-overhaul`
- Base commit: `d2071d2 v0.21.2`
- Seeded changes: current Codex map/hazard patch, including archive progress/event-click loading, place search state labels, per-pane info bars, debug cases, Vrot hardening, and SPC valid-now Day 1 fix.
- Baseline test: `cargo test -p app_ui` passed with `288 passed; 0 failed; 10 ignored`.

## Current Verification

- Workflow preset focused test: `cargo test -p app_ui workflow -- --nocapture` passed with `10 passed`.
- Layout focused tests: `cargo test -p app_ui pane_cell_rects --no-default-features` and `cargo test -p app_ui four_grid_has_four_panels --no-default-features` passed.
- Timezone focused tests: `cargo test -p settings time_zone --no-default-features`, `cargo test -p app_ui display_time_zone --no-default-features`, and `cargo test -p app_ui cursor_readout_format --no-default-features` passed.
- Current-alerts focused tests: `cargo test -p app_ui current_alert --no-default-features`, `cargo test -p app_ui hazard_focus_view --no-default-features`, and `cargo test -p app_ui hazard --no-default-features` passed; the hazard slice now covers 21 tests.
- Customization focused tests: `cargo test -p app_ui custom_tab --no-default-features`, `cargo test -p app_ui color_table_manager --no-default-features`, `cargo test -p app_ui map_backdrop --no-default-features`, `cargo test -p app_ui hazard_polygon_style`, and `cargo test -p styles map_background --no-default-features` passed.
- Radar marker focused tests: `cargo test -p app_ui terminal_site_markers --no-default-features`, `cargo test -p app_ui loaded_site_marker --no-default-features`, and `cargo test -p app_ui freshness_ring_color --no-default-features` passed.
- Radar-label focused tests: `cargo test -p app_ui radar_label -- --nocapture` and `cargo test -p settings radar_labels -- --nocapture` passed; Custom -> Map layers now has a persisted Radar labels toggle that hides WSR-88D and TDWR/Txxx label boxes while keeping radar dots/click targets visible.
- Loop playback focused tests: `cargo test -p app_ui loop_playback -- --nocapture` passed; Space now toggles loaded-loop play/pause through the same state transition as the Play/Pause button when no text field is focused.
- Radar age focused tests: `cargo test -p app_ui radar_age_glyph_arc -- --nocapture`, `cargo test -p app_ui mode_chip_uses_radar_age_style_threshold_and_colors -- --nocapture`, and `cargo test -p app_ui freshness_ring_color -- --nocapture` passed; loaded radar markers now draw the same scan-age style as a fixed-size site arc, the mode chip uses the same style registry as the scan-age ring, and future scan labels clamp to `0m old`.
- Guide/discoverability focused tests: `cargo test -p app_ui guide_copy --no-default-features` and `cargo test -p app_ui custom_tab_advertises_layers_and_appearance -- --nocapture` passed; the guide and tab tooltip guards reject stale Layers/Settings-color-table copy and keep radar-age customization discoverable.
- Alert-sound focused tests: `cargo test -p settings alert_sound --no-default-features` and `cargo test -p app_ui alert_sound --no-default-features` passed.
- Alert-flash focused tests: `cargo test -p settings alert_flash --no-default-features`, `cargo test -p app_ui visual_alert --no-default-features`, and `cargo test -p app_ui install_hazard_result_latches_only_visual --no-default-features` passed.
- Style-reset focused test: `cargo test -p app_ui reset_style_overrides --no-default-features` passed.
- Map tile cache focused tests: `cargo test -p app_ui clear_map_tile_cache --no-default-features` and `cargo test -p app_ui tiles::tests --no-default-features` passed.
- Top-tool focused test: `cargo test -p app_ui annotation_target_cell_prefers_hovered_pane_and_clamps_fallback -- --nocapture` passed.
- Cross-section focused test: `cargo test -p app_ui cross_section_handle -- --nocapture` passed with `1 passed`.
- Async/status focused tests: `cargo test -p app_ui background_activity -- --nocapture` passed with `8 passed`; `cargo test -p app_ui load_worker_disconnect -- --nocapture` passed with `1 passed`.
- Archive/event persistence focused tests: `cargo test -p settings archive_controls -- --nocapture`, `cargo test -p settings event_pad -- --nocapture`, `cargo test -p app_ui archive_frame_count_clamps -- --nocapture`, `cargo test -p app_ui event_pad -- --nocapture`, `cargo test -p app_ui cached_event_frame_selection -- --nocapture`, `cargo test -p app_ui event_track_load_plan -- --nocapture`, and `cargo test -p app_ui archive_ -- --nocapture` passed.
- Workspace reset focused test: `cargo test -p app_ui workspace_reset_clears_persisted_layout_and_grid_state` passed.
- Data-folder reset focused tests: `cargo test -p settings data_dir -- --nocapture` and `cargo test -p app_ui data_folder -- --nocapture` passed.
- Custom poll/GIS focused tests: `cargo test -p settings custom_poll -- --nocapture`, `cargo test -p app_ui custom_poll -- --nocapture`, `cargo test -p app_ui custom_radar_gis -- --nocapture`, and `cargo test -p app_ui best_radar_candidates_include_custom_poll_links -- --nocapture` passed. Link-only saved poll roots now work without marker coordinates, while coordinate-backed custom feeds still draw red markers and participate in nearest-radar ranking.
- Product/cut intent focused tests: `cargo test -p app_ui sanitize_selection -- --nocapture`, `cargo test -p app_ui product -- --nocapture`, `cargo test -p app_ui velocity_cut -- --nocapture`, and `cargo test -p data_source ord -- --nocapture` passed.
- ORD same-elevation product pairing focused test: `cargo test -p data_source ord -- --nocapture` passed with the new `newer_scan_tail_with_mismatched_product_heights_does_not_win` regression, so a newer SCAN tail with reflectivity and velocity at different heights no longer displaces an older coherent frame.
- Map modifier-click focused test: `cargo test -p app_ui modified_map_clicks_are_exclusive_from_plain_marker_clicks` passed.
- Map report-click focused test: `cargo test -p app_ui plain_map_click_routes_reports_only_when_no_marker_is_hit` passed.
- Place-search focused tests: `cargo test -p app_ui place_search -- --nocapture` and `cargo test -p app_ui basemap -- --nocapture` passed.
- Inspector pin focused test: `cargo test -p app_ui inspector_pin -- --nocapture` passed.
- Radar overlays window focused tests: `cargo test -p app_ui radar_overlays -- --nocapture` passed with `2 passed`.
- Security/update focused test: `cargo test -p app_ui security_updates -- --nocapture` passed.
- Diagnostics focused tests: `cargo test -p app_ui diagnostic -- --nocapture`, `cargo test -p app_ui security_updates -- --nocapture`, `cargo test -p app_ui clear_map_tile_cache -- --nocapture`, and `cargo test -p app_ui background_activity -- --nocapture` passed.
- Startup renderer fallback focused tests: `cargo test -p app_ui wgpu_device_lost_startup_error_retries_with_glow -- --nocapture`, `cargo test -p app_ui diagnostic_summary_includes_support_context -- --nocapture`, and `cargo test -p app_ui security_updates -- --nocapture` passed; WGPU DeviceLost startup failures now retry with OpenGL/Glow and leave a support-visible startup notice.
- Formatting: `cargo fmt --all --check` passed.
- Lint: `cargo clippy -p app_ui --all-targets` passed with no warnings after documenting the intentional plain-map click router arity and cleaning up test modifier setup.
- Data-source lint: `cargo clippy -p data_source --all-targets -- -D warnings` passed after the ORD same-elevation planner hardening.
- Full styles test: `cargo test -p styles` passed with `11 passed`.
- Full settings test: `cargo test -p settings` passed with `18 passed`.
- Full app UI test: `cargo test -p app_ui` passed with `358 passed; 0 failed; 10 ignored` in `src/main.rs`, plus the `src/lib.rs` pane test.
- Full data-source test: `cargo test -p data_source` passed with `87 passed; 0 failed; 4 ignored`.
- Release build: `cargo build --release -p app_ui` completed after the cached event-frame progress cleanup, cached-track overlay planning, archive busy-load intent preservation, Clippy cleanup, Documentation workflow cleanup, stale Settings tooltip cleanup, radar-age customization, loaded-site marker arc, radar-label toggle, Space loop play/pause shortcut, v0.22.0 version bump, dynamic macOS bundle versioning, international ODIM/merge/startup fixes, custom poll-link, GR radar GIS import, split-product cut/product selection, copy-diagnostics, international place-search, WGPU fallback notice, ORD same-elevation planner, and link-only custom poll slices.
- Codex executables:
  - `C:\Users\drew\radar-work\wt-codex-robustness\target\release\bowecho-codex-robustness.exe`
  - `C:\Users\drew\radar-work\wt-codex-robustness\target\release\bowecho-codex.exe`
- SHA256: `3B13F799D7CD5392D7C797855CE81F639C17AC87C6E0D076FCCEC073F5685810`

## Already Present In Current App

- Five sidebar tabs: Radar, Custom, Severe, Data, Settings.
- Custom tab with separate Map layers, Add layer, Analysis overlays, and Appearance sections.
- Layer rail and Add layer menu.
- Windows menu in the top bar.
- Windows -> Radar overlays opens a dockable/floating manager for extra radars. It reuses the same overlay-row controls as Custom: visibility, opacity, refresh, center, promote to primary, and remove, with an empty-state hint for Ctrl-right-click / Custom -> Add layer.
- Clean-screen mode with the top-bar `Map Only` button plus Tab/Esc restore/toggle behavior.
- Pane layouts: 1, 2, 3, and 4 synchronized map panes.
- Workspace layout persistence and dockable viewer panes.
- Settings -> Display has a scoped `Reset layout` action that clears saved dock layout JSON, pending pane changes, 2/3/4-pane state, and stale dock requests while keeping currently visible docked viewers open as floating windows for the current session.
- Settings -> Security & updates shows the running version, release-check state, official releases link, manual check button, and clear Defender/SmartScreen copy explaining unsigned/new executable warnings without claiming in-app signature verification.
- Settings sections with persistent open state.
- Sidebar section headings are bold/brighter with stronger separators for faster scanning.
- Style registry and `styles.json`-backed customization plumbing.
- Persistent map backdrop color under Custom -> Appearance.
- Custom -> Appearance has a warning-polygon style editor for global fill/width/selected emphasis plus per-family stroke color, fill color, line width, dash style, and reset; edits flow through the same `styles.json` registry used by live alert rendering.
- Custom -> Appearance has radar-age controls for the data-edge age ring, loaded-site marker age arc, age thresholds, LIVE/STALE chip threshold, and the fresh/aging/stale/expired colors used by loaded radar markers and the map chip.
- Custom -> Appearance has a scoped `Reset all` style override action that restores built-in map/style defaults without touching other settings; it is disabled for newer-schema `styles.json` files.
- Color table persistence and per-product palette overrides.
- Color tables live under Custom -> Appearance only; the duplicate Settings -> Color tables section and stale guide breadcrumb were removed.
- Data-folder override with scoped Default reset: changing or clearing it updates only the persisted data-folder path and reports that restart is required; live stores do not move mid-session.
- Data -> Live feeds has a saved custom-poll link list for private/mobile radar roots. Entries store label, optional site id, poll root, and optional marker lat/lon; bare hosts/IPs are normalized to `http://`, entries persist in `config.json`, link-only entries can be polled from the saved list, red map markers click-to-poll coordinate-backed saved roots, and coordinate-backed custom entries participate in the lowest-beam/right-click radar ranking. `Import GIS...` accepts GR customradars/radars GIS rows in comma or whitespace form, uses the current Poll URL as the base root, expands multi-site imports as `base/site`, and supports `{site}` / `{SITE}` / `{id}` / `{ID}` placeholders for explicit URL templates.
- Product/cut selection keeps product intent across split international scans: if the selected product is not on the current tilt but exists on another tilt, selection sanitization moves to the nearest valid tilt instead of silently falling back to reflectivity on the current cut.
- Settings -> Performance has a scoped `Map tile cache -> Clear` action that deletes only cached raster basemap tiles, clears in-memory tile state, and refuses non-`tiles` paths.
- Settings -> Performance has `Copy diagnostics`, a paste-ready support snapshot with version, OS/arch, renderer backend, map/selection/frame state, active background work, loaded source, poll source, workers, timing, and config/cache/store paths.
- Startup renderer fallback: BowEcho first tries WGPU; if startup fails with WGPU/RequestDevice/DeviceLost/RequestAdapter errors, it retries with OpenGL/Glow instead of exiting. The fallback is recorded as a startup notice and included in Copy diagnostics.
- Units setting: imperial/metric.
- Display timezone setting: UTC, Eastern, Central, Mountain, or Pacific. Data fetch keys stay UTC.
- Place search finds US cities/towns with state disambiguation and now searches major world/regional basemap places plus Canada/Mexico/Japan city/admin labels; suffixes like `portland or`, `tokyo jp`, `toronto canada`, and `mexico city mexico` filter the result list.
- Settings -> Alerts: visual flashing/NEW latch can be enabled/disabled and scoped by warning family; audible cue is opt-in with custom `.wav` picker, system-alert fallback, Test button, and separate family toggles for tornado, severe thunderstorm, and flash flood.
- Severe tab Current alerts list: visible/current warning polygons are browsable and click-to-jump/select.
- New-alert attention latch: later live hazard refreshes mark added active/current polygons as new when their family is enabled, flash a top-bar alert chip, and clear the latch when the row/polygon is clicked or when visible alerts are acknowledged.
- Event Explorer archive loop loading with track/report jumps; cached event-frame hits select immediately without leaving a stale background progress bar active, cached primary track hits still queue the second-radar overlay, and report/track jumps survive a listing result landing while another primary decode is busy.
- Loaded frame loops can be played/paused with Space when no text field is focused; the Play button tooltip advertises the shortcut.
- Archive browser controls persist across restarts: `Fetch N scans` and the Loop/Single click mode are stored in `config.json`, restored on startup, and clamped to the supported 1-30 scan range.
- Event Explorer track-click context persists across restarts as `event_pad_frames`; hand-edited values are clamped to the same 0-40 scan range as the UI before archive loops use them.
- Status bar background activity summary: active radar/archive loads, Event Explorer day fetches, overlay loads/renders, model ingest, obs analysis/OA derived/composites, international site catalogs/loops, placefile/icon fetches, storm/rotation analysis, satellite/model layer builds, WoFS catalog/image/station/georef work, FARM frame/quicklook/locator work, 3D volume resampling, polling, hazards, SPC/RAOB/obs/sounding/catalog workers get a spinner or progress bar, and active work schedules modest repaints so progress does not appear frozen when a detailed panel is closed.
- Debug cases section with KBMX Tuscaloosa 2011 22:15Z and 22:19Z.
- Vrot tool guard for velocity-only products and multi-pane click support.
- Cross-section endpoint handles own pointer input across single and grid panes, so refining a drawn slice does not also pan/select/restart the map underneath.
- Annotation tools now own pointer input and render saved/draft graphics in 1/2/3/4 pane layouts; active annotations also dismiss the community-feed picker so it cannot shadow map drawing.
- FARM manual radar placement now owns right-click in grid panes instead of accidentally opening/load-switching through the best-radar menu.
- Modified map clicks are exclusive and work in grid layouts: Alt-click/Ctrl+Alt hover model soundings and Ctrl-click lowest-beam radar jump no longer fall through to ordinary marker/hazard clicks, and Ctrl-right-click overlay adds no longer also open the lowest-beam context menu.
- Shift-click inspector pinning works from any grid pane, not only the primary pane; the pinned card remains tied to the clicked geo point.
- Plain map clicks share report/track routing in single and grid layouts: SPC report dots and tornado tracks load archive/event review when no radar/feed marker is under the cursor.
- CONUS radar markers: zoom-gated site/name labels, selected-site labels, terminal/TDWR amber markers with yellow-backed labels, and loaded primary/overlay radar markers colored by scan age with a fixed-size age arc.
- Custom -> Map layers has a persisted `Radar labels` toggle. Turning it off hides WSR-88D and terminal/TDWR `Txxx` label boxes while preserving marker dots, hover/click behavior, selected site state, and range rings.
- SPC live Day 1 valid-now handling for the 0100Z overnight product.
- Built-in Workflows menu with session presets, a session-only current-workflow chip, a Restore previous setup action, and a Clear marker action:
  - Live severe
  - Triple severe
  - Velocity couplet
  - Quad dual-pol
  - Archive review
  - Documentation
  - Model context
- Archive review workflow now forces Loop mode back to at least the default 10-scan fetch count and updates the in-memory archive/pane settings state, so a previous single-frame review setting cannot neuter the preset.
- Workflow presets are now state-scoped and reversible for the setup state they touch: pane layout/products, sidebar tab, overlays, live/archive controls, model window state, clean-screen state, inspector/Vrot/cross-section/annotation arm state, temporary cross-section endpoints, in-progress annotation drafts, and the relevant settings mirror.
- Documentation workflow now hides temporary cross-section measurements and in-progress annotation drafts for clean map capture while preserving saved annotations and restoring the prior temporary state through Workflows -> Restore previous setup.
- Guide copy now reflects Custom instead of old Layers wording, 1/2/3/4 pane layouts, Workflows, Map Only, radar-age styling, Settings -> Alerts, and Settings -> Debug cases.
- Custom/Layers/Settings user-facing wording is aligned: Radar quick links say `Custom: N layers`, the satellite map-layer hover points to Custom, Custom advertises radar age/appearance/color tables, Settings no longer advertises color tables, and regression tests guard against stale `Layers: N` and Settings-color-table copy.

## Robustness Definition

No app can honestly be proven bug-free, so the practical bar is:

- Every visible control either works, is disabled with a clear reason, or links to the owning workflow.
- All async loads show progress or a clear status.
- User intent is never silently overridden by auto-refresh, stale workers, pane focus changes, or cached frames.
- Multi-pane behavior matches single-pane behavior unless intentionally different.
- Live, archive, local file, and debug-case paths converge through shared install logic where possible.
- Every non-trivial bug fix gets a unit test, a regression fixture, or a manual QA recipe.
- Release artifacts are produced only after full tests and a release build.

## First Audit Buckets

### P0: Broken Or Misleading Workflows

- Audit all armed tools in single, 2-pane, 3-pane, and 4-pane layouts: Vrot, cross-section, annotations, FARM placement, model Alt-click sounding, Ctrl-click radar jump, Shift-click inspector pin.
- Audit archive/event loading: click report dot, tornado track, future/cached frame, loop range extension, single-scan mode, progress display, failure display.
- Audit live-vs-archive SPC behavior around 01Z, 06Z, 12Z, and archive dates.
- Audit Windows Defender/signing/update messaging so users know what is trust/signed vs unsigned/false-positive.
- Audit all context menus and right-click modes so right-click never triggers two actions.

### P1: Make Features Discoverable And Complete

- Add or verify workflow presets: Live Severe, Archive Case Review, Velocity/Couplet, Documentation/Clean Map, Quad Dual-Pol, Model Context.
- Make presets state-scoped and reversible: pane layout, selected products, sidebar tab, overlays, windows, loop state, and clean-screen/chrome state. Done: Workflows -> Restore previous setup restores the pre-preset setup snapshot for the latest workflow command.
- Add a visible current-workflow chip or menu entry only if it does not crowd the top bar.
- Add Settings/Guide entries for debug cases and workflow presets.
- Ensure every layer row gear opens the right owning surface.

### P1: State Persistence And Reset Safety

- Verify settings save/load for overlays, pane count, workspace layout, favorites, color tables, styles, model slug, units, archive/event pads, poll URLs, custom poll links, and FARM georefs.
- Add reset actions that are scoped: reset layout, reset styles, reset data folder, reset workflow, clear cache candidate.
- Verify newer-schema behavior for config/styles stays protective.

### P2: Fluidity And Responsiveness

- Audit all network/IO decode paths for UI-thread blocking.
- Confirm every long-running worker requests repaint and reports progress.
- Check map pan/zoom render behavior under heavy layers and multiple panes.
- Profile common workflows: latest L2 load, archive loop load, product switch, pane switch, layer toggle, model layer render.

### P2: Edge Case Matrix

- No volume loaded.
- Load in flight.
- Stale/old archive volume.
- Different site loaded while loop/history has frames.
- Live partial volume missing selected product.
- International/non-NEXRAD volume with missing moments or low radial counts.
- Local file/folder/mobile radar load.
- Missing network, 404, corrupt file, empty decoded product set.
- Very small and very large window sizes.
- 1/2/3/4 pane layouts, including focused extra panes.

## Candidate Implementation Order

1. Build an app-surface inventory from the actual current code.
2. Add workflow preset data model and a small UI entry point. Done: top-bar `Workflows` menu.
3. Wire the first presets using existing state transitions. Done: seven built-in session presets, including a 3-pane severe-review preset.
4. Add regression tests around workflow state application. Done: label uniqueness, velocity preset, triple severe preset, quad preset, and pane geometry.
5. Add a persisted USA display-timezone preference. Done: Settings -> Display, map chips, pane bars, frame history, cursor readout, SPC report labels, RAOB titles, and selected model-valid status honor the display zone while archive fetch keys remain UTC.
6. Add a Severe tab Current alerts workflow. Done: list honors active/family/renderability filters, selecting a row highlights details and recenters/zooms the map to the polygon.
7. Add workflow discoverability state. Done: applying a preset records a session-only marker, displays the current workflow chip in the top bar, Workflows -> Restore previous setup reverts the latest preset's setup changes, and Clear marker hides only the marker without reverting applied state.
8. Add warning-documentation attention state. Done: newly added active/current hazard polygons latch a flashing alert chip until selected or acknowledged.
9. Rebucket customization. Done: the user-facing Layers tab is now Custom with Map layers, Add layer, Analysis overlays, and Appearance; Appearance includes color tables, warning-polygon styles, and a persistent map backdrop color.
10. Improve radar-site marker readability and freshness cues. Done: terminal/TDWR sites are visually distinct, selected and zoomed labels include site names where known, and loaded primary/overlay sites use scan-age coloring.
11. Refresh Guide discoverability. Done: Guide navigation and copy now match the Custom tab, 3-pane layout, Workflows, Map Only, Settings -> Alerts, and Settings -> Debug cases.
12. Add opt-in warning sound workflow. Done: persisted Settings -> Alerts controls, custom WAV/system fallback, warning-family gates, and new-alert latch trigger on Windows.
13. Add configurable warning flashing. Done: persisted Settings -> Alerts visual flashing switch and warning-family gates; disabled flashing clears the current unacknowledged visual latch without changing sound settings.
14. Add scoped style reset safety. Done: Custom -> Appearance can reset all sparse style overrides back to built-in defaults, with newer-schema protection and a registry-reset regression test.
15. Add scoped map tile cache cleanup. Done: Settings -> Performance can clear cached Satellite/Streets/Topo tiles only, with a path guard and regression tests.
16. Persist archive/event review controls. Done: `Fetch N scans`, Loop/Single click mode, and Event Explorer track-click context scans now round-trip through settings with app-side range clamps.
17. Harden workspace reset safety. Done: `Reset layout` now clears persisted dock JSON and pane-count state instead of letting the debounce writer re-save the old/current layout after reset.
17a. Harden data-folder reset safety. Done: Settings -> Display -> Data folder `Default` is disabled when already default, clears only `data_dir` when used, preserves other settings, and reports restart-required status with app/settings regression tests.
18. Audit/fix top tool interactions across pane layouts. In progress: annotation drawing, cross-section endpoint drags, FARM right-click placement, model Alt-click/Ctrl+Alt soundings, Ctrl-click lowest-beam radar jump, Ctrl-right-click overlay adds, Shift-click inspector pinning, and SPC report/tornado-track clicks now match the single-pane ownership rules in grid layouts.
19. Audit async progress/status gaps and fix the highest friction ones. In progress: status bar now centralizes active background work, including Event Explorer day fetches, international site/loop, placefile, obs-analysis, storm-track, rotation-marker, WoFS, FARM, and 3D-volume workers, and keeps repainting during long-running jobs even when their detailed panel is closed.
20. Produce a fresh `bowecho-codex-robustness.exe`. Done: see Current Verification.
21. Add customizable warning-polygon styling. Done: Custom -> Appearance edits global alert polygon emphasis plus per-family colors/width/dash, with Guide discoverability and style-registry regression tests.
22. Dedupe color-table navigation. Done: Settings no longer carries a duplicate Color tables section; product palette edit paths and Guide copy point to Custom -> Appearance.
23. Add a radar-overlays window. Done: `Windows -> Radar overlays` opens/docks a first-class overlay-radar manager so separate radar sources are no longer buried only in the Custom layer list.
24. Add Defender/signing/update messaging. Done: Settings -> Security & updates surfaces version/update state, official release access, manual recheck, and explicit unsigned-build guidance for Windows Defender/SmartScreen reports.
25. Expose radar-age customization. Done: Custom -> Appearance can tune the radar age ring, loaded-site marker arc, age thresholds, chip threshold, and colors; the LIVE/STALE/ARCHIVE chip and loaded marker age surfaces now read those resolved style settings instead of hard-coded colors/thresholds.
26. Add custom polling link lists and custom radar markers. Done: DATA -> Live feeds now saves direct GR2A-style poll roots with label/site/lat/lon metadata, draws them as red map dots, starts them from list or marker clicks, normalizes bare IP/host inputs, includes them in the lowest-beam radar menu, and imports GR customradars/radars GIS text files into saved entries.
27. Harden split-product cut/product selection. Done: selection sanitization now preserves the user's product choice and jumps to the nearest displayable cut when reflectivity/velocity/dual-pol moments live on different tilt sets, matching the existing render/pane `best_cut_for_product` behavior.
28. Add copyable diagnostics. Done: Settings -> Performance copies a compact support snapshot, including WGPU/Glow backend, active background progress, current source/selection/history/poll state, timing, worker flags, and relevant app paths with custom poll URL paths redacted to host-level.
29. Broaden place search. Done: search now uses the US city/town catalogs plus world major-city and regional Canada/Mexico/Japan city/admin labels, keeps US state disambiguation, adds country context labels, and accepts country suffix filters without stealing state abbreviations like `CA`.
30. Harden WGPU startup failures. Done: WGPU/RequestDevice/DeviceLost/RequestAdapter startup errors retry with the Glow renderer, and the fallback reason is visible in status/diagnostics for support reports.
31. Harden ORD SCAN product pairing. Done: the frame planner now treats a reflectivity+velocity candidate as complete only when the chosen files share at least one elevation, preventing newer partial SCAN tails from pairing reflectivity at one height with velocity at another; velocity-only/ref-only sites still fall back to the newest available frame.
32. Harden custom poll link-only workflows. Done: Data -> Live feeds can save a GR2A-style poll root without lat/lon for private feeds that should be list-only, while marker-backed feeds still draw red dots and participate in nearest-radar ranking.
33. Add radar-label declutter toggle. Done: Custom -> Map layers can hide all CONUS radar labels, including terminal/TDWR `Txxx` labels, without hiding the markers themselves.
34. Add Space loop play/pause. Done: Space toggles loaded-loop play/pause through the shared playback helper and is ignored while text fields are focused.
35. Prepare v0.22.0 release metadata. Done: workspace version is `0.22.0`, and the macOS release workflow writes `CFBundleShortVersionString` / `CFBundleVersion` from the tag (or Cargo version on manual dispatch) before signing/notarization.

## Handoff Notes

- Do not work in `wt-codex-map-hazards` for this goal except to compare or recover the seed patch.
- Keep this branch dirty only with the current goal's work.
- Before merging elsewhere, separate the seeded map/hazard patch from new robustness work if a clean PR split is needed.
- Full idle-marker stale-before-click coloring still needs source metadata: `RadarSite` does not currently carry latest scan timestamps, so the app colors loaded primary/overlay sites by real volume age and does not invent freshness for unloaded sites.
- Live ORD spot-check on 2026-06-13: the public bucket/API still lists France 20, Belgium 2, Estonia 1, Iceland 3, Poland 10, and Ireland 2 sites for the active rolling cache. The user-named missing IDs like `frtra`, `behel`, `eehar`, and `isx1` were not present in that source at check time, so custom GIS/poll entries are currently the practical path for those until ORD publishes them.
- ORD frame planning now prefers the newest cycle where reflectivity and velocity overlap by elevation. This should reduce international cases where velocity appears to cover a different scan height than reflectivity, without blocking velocity-only feeds like Dublin.
