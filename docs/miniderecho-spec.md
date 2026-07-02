# miniDerecho v1 — Radar-Only Companion App Spec

**Status:** the miniDerecho v1 program. Written against branch `v028/unslop` @ `89db485`
(v0.29 Phase 1A+1B landed: `crates/app_ui/src/worker_slot.rs` = 456 lines with tests;
`crates/data_source/src/sites.rs` = 541 lines, Phase 1C; `crates/app_ui/src/main.rs` = 71,059 lines).
Line numbers WILL drift under the write fleet — every reference names the symbol, which is durable.
**Re-verify lines by grep at implementation time.**

**Inputs:** a two-way judged design panel (crate-plan/engineering-shape design; UX-blueprint design)
plus an adversarial feasibility challenge that verified every reuse anchor against this tree and the
locked dependency versions (`Cargo.lock`: eframe/egui/egui-winit 0.34.3, winit 0.30.13, glutin 0.32.3,
wgpu 29.0.3 already resolved). This spec is the synthesis: the crate plan, dependency firewall,
pipeline, and sizing discipline come from the engineering design; the layouts, bar, gestures,
first-run chain, live-edge state machine, and warnings UX come from the UX design; every blocker the
challenge PROVED is resolved in §1 with its cheapest resolution. Where the two designs disagreed
(intl-in-v1, geolocation, crate granularity, sizing), the decision is recorded inline with its reason.

**Product frame (owner-decided — do not re-litigate):** miniDerecho is "the RadarScope of desktop
radar apps." A SEPARATE app — own bin, name, icon, releases — sharing BowEcho's workspace crates.
v1 surface is RADAR-ONLY MINIMAL: live radar, loops, tilts, core products, warnings. One responsive
app: phone layout (bottom sheets, gestures, big targets) vs desktop layout (thin panels), detected by
form factor at runtime, never by build flag. It is the future iOS app; the ONE hard constraint is RAM
(the ~4.4 GB HRRR-ingest class kills iOS — irrelevant for radar-only v1, but the ceiling shapes every
cache). Beauty and first-run simplicity are product features: launch → see radar within seconds,
zero configuration.

---

## 0. End-state invariants (what "done" means for v1)

1. `miniderecho.exe` exists as its own release artifact with its own icon/VERSIONINFO; BowEcho's
   behavior is unchanged by every extraction commit (verbatim moves, `git diff` shows pure moves).
2. `cargo tree -p mini_ui -e normal` contains **no** `rw-*`, `rustwx-*`, or `sharprs` crate and no
   `app_ui` — enforced by a CI gate, so the HRRR-class ingest is structurally unreachable, not merely
   unused.
3. Every background operation in mini is a `WorkerSlot`/`StreamSlot` (now shared via `ui_core`) —
   mini is born with v0.29's channel-ownership invariant (v029-engine-spec.md §0 invariant 3) and
   never grows the `Option<mpsc::Receiver>` idiom.
4. Frame retention is byte-budgeted from the first commit (`HistoryLimits.byte_budget` semantics,
   which mini ships FIRST); steady-state RSS on the phone profile stays under the §7 table.
5. Every displayed time comes from data, never wall clock; every unavailable affordance is greyed
   with a reason that IS the value a capability function returned (house trust-label rule).
6. Every behavior is tested (house discipline): startup chain, form-factor switch, gesture
   classifier, live-edge state machine, byte-budget trim, warning filter/hit-test, settings
   round-trip — all pure functions with unit tests.
7. The About screen carries the credit policy: NOAA/NWS/Unidata Level-II, Esri tile attribution
   (the strings already in tiles.rs), color-table credits, ambient330 (MIT), grayskieswx handle-only;
   coordinate contributor + field testers uncredited by request; never real names. Plus the escape
   hatch line: "Need more? BowEcho is the power app."
8. mini adopts each v0.29 shared engine as it lands (§10): SiteRef NOW, ArchiveFrames at v0.29
   Phase 2 (v2 feature), RenderService at Phase 4a/4b, LoopEngine at Phase 4c — each adoption
   deletes a named mini placeholder.

---

## 1. Read this first — five PROVED blockers and their resolutions

The feasibility challenge verified these against the tree and `Cargo.lock`. Each resolution below is
binding for this program.

### B1. The glow backend CANNOT run on iOS (certain, not "likely")

glutin 0.32.3 ships only `{cgl, egl, glx, wgl}` context APIs; CGL is macOS-only and iOS has no
system EGL — eframe's glow path has no context-creation route on device even though eframe 0.34.3
compiles `run_glow` for `target_os = "ios"`. **Resolution:** `mini_ui/Cargo.toml` declares
per-target eframe features:

```toml
[target.'cfg(not(target_os = "ios"))'.dependencies]
eframe = { workspace = true }                                   # glow — matches BowEcho exactly
[target.'cfg(target_os = "ios")'.dependencies]
eframe = { version = "0.34.3", default-features = false, features = ["wgpu", "wayland"] } # wgpu → Metal
```

Zero new dependencies: eframe already resolves WITH egui-wgpu + wgpu 29.0.3 in this workspace's
Cargo.lock. Desktop mini stays on glow (BowEcho's proven backend); the iOS leg is wgpu, consistent
with docs/ios-port-notes.md ("Graphics is wgpu → Metal") and the owner's successful iPhone 16 Pro run.

### B2. v0.29 spec places LoopEngine/RenderService INSIDE the app_ui bin as `pub(crate)`

docs/v029-engine-spec.md specs `crates/app_ui/src/loop_engine.rs` (`pub(crate) struct LoopEngine`)
and `crates/app_ui/src/render_service.rs` (§3, §4.2, §6 rows 6-7). mini structurally cannot import
either: app_ui's Cargo.toml drags 12 `rw-*`/`rustwx-*`/`sharprs` git deps (crates/app_ui/Cargo.toml:57+),
and its lib target is a 34-line PanelLayout stub. **Resolution — Coordination Prerequisite CP-1:**
amend v029-engine-spec.md §6 rows 6-7 (and §3/§4.2 headers) so `render_service.rs` and
`loop_engine.rs` land in the shared **`crates/ui_core`** crate with `pub` types; app_ui is the first
consumer, mini the second. This is a doc edit today (no code exists against the old locations yet)
versus a second extraction later. The FrameHistory/LoopEngine/RenderService/LaneId designs are
already shell-agnostic (§3: "the engine never reaches into siblings"); nothing in their spec'd API
mentions ViewerApp. **CP-1 must be applied to v029-engine-spec.md before v0.29 Phase 4 starts.**
The `ui_core::worker_slot` move in mini's M0 establishes the precedent and the migration mechanics.

### B3. `inject_for_test` is `#[cfg(test)]` — invisible after extraction

`WorkerSlot::inject_for_test` / `StreamSlot::inject_for_test` (worker_slot.rs:164-168, :290-295) are
`#[cfg(test)]`; once worker_slot moves to `ui_core`, mini_ui's (and app_ui's) tests could not call
them. **Resolution:** in the move commit, change the gate to
`#[cfg(any(test, feature = "test-util"))]`; `mini_ui` and `app_ui` add
`ui_core = { workspace = true, features = ["test-util"] }` under `[dev-dependencies]`. Two-line diff.

### B4. The settings-document trap

`AppSettings` persistence is hard-wired to `bowecho_config_dir()/config.json`
(settings/src/lib.rs:708, :831), and the storage-namespace machinery DELIBERATELY excludes the
settings/styles documents ("deliberately stay in the legacy BowEcho root", :836-838; same note for
the data-dir override at :937-941). `set_storage_namespace("miniderecho")` namespaces caches/tiles,
but any reuse of `AppSettings::load/save` would write into BowEcho's config.json — and
`data_dir_override()` reads `BOWECHO_DATA_DIR` directly from the environment (:948), so mini would
silently honor BowEcho's data-dir override. **Resolution:**

- One additive helper in `crates/settings`:
  `pub fn config_path_for_namespace(namespace: &str) -> Option<PathBuf>` =
  `storage_root_for_namespace(namespace)?.join("config.json")`. Mini's config path is
  `config_path_for_namespace("miniderecho")`. `MiniSettings` (own Eq-derivable struct, lives in
  `mini_ui`) NEVER touches `AppSettings::load/save`.
- Mini calls `settings::set_storage_namespace(Some("miniderecho"))` at boot (caches/stores land
  under the mini namespace).
- **Decision:** mini does NOT honor `BOWECHO_DATA_DIR`. It reads `MINIDERECHO_DATA_DIR` itself in
  `mini_ui` and passes explicit directories down. To make this airtight, mini never calls
  `settings::tile_cache_dir()` (which routes through the BowEcho env override): the extracted
  `ui_core::tiles::TileLayer` takes its cache directory as a constructor parameter (§2, ui_core).

### B5. `cargo check --target aarch64-apple-ios` cannot run on Windows/Linux runners

bzip2-sys and ring run cc-based build scripts during `check` and need an Apple SDK; cross-checking
apple-ios targets from non-macOS hosts fails at the build-script stage. **Resolution:** the iOS
tripwire is a **macOS GitHub Actions job** (`runs-on: macos-latest`;
`rustup target add aarch64-apple-ios && cargo check --target aarch64-apple-ios -p mini_ui`) — needs
no physical Mac session, just a macOS runner leg. Lands at M1 (once mini_ui exists in CI at all).

### Anchor corrections adopted from the challenge (use these, not the panel drafts')

`dedupe_hazard_records` = main.rs:46879 (NOT :46551, which is the `build_live_hazard_overlay`
region) · `best_radar_candidates` :28385 · `mode_chip_state` :36644 · eframe pin = workspace
Cargo.toml:15 · `analyst_hd_velocity_table` = color_tables/src/lib.rs:970 ·
`merge_render_request` :12738 · worker_slot.rs = 456 lines ·
`ViewportRasterOptions` has `km_per_px_x` AND `km_per_px_y` (render2d/src/lib.rs:99-111), not a
single `km_per_px` — the phone internal-resolution cap uses both fields ·
`parse_weather_alert_feature` is `#[cfg(test)]`-only (:47547); the production entry point is
`parse_weather_alert_feature_with_zones` (:47555) — the hazards crate's public API takes the
`_with_zones` shape (callers pass an empty zone map to skip zone polygons) ·
`latest_realtime_level2_volume(site: &str)` takes the US Level-II id string, not a SiteRef —
consistent with the §3 v1 scoping (US-only live path).

---

## 2. Crate plan

Workspace members glob `crates/*`; four additions. Extraction playbook = layers_rail / worker_slot:
verbatim moves, extraction commits never mixed with behavior commits, each landed within days.

### `crates/ui_core` — shared egui-shell library (NEW, M0)

Deps: `eframe`, `image`, `data_source`, std. Feature `test-util` (B3). This crate is also the
**CP-1 landing pad** for v0.29's RenderService/LoopEngine/FrameHistory extractions.

- **`worker_slot.rs`** — moved VERBATIM from `crates/app_ui/src/worker_slot.rs` (456 lines incl.
  tests). Diffs allowed in the move commit, exhaustively: (a) `pub(crate)` → `pub` on every item;
  (b) the B3 `test-util` gate; (c) doc comments that name BowEcho-specific functions
  (`cancel_extra_pane_load_for_user_command`, `run_self_update_worker`) rephrased to be app-neutral.
  BowEcho migration: delete `mod worker_slot;`, add the ui_core dep, rewrite the single use-site
  (`use worker_slot::{...}` at main.rs:77) — a one-commit, test-covered move.
- **`tiles.rs`** — moved VERBATIM (489 lines): TileLayer/TileStyle/TileId, tile math
  (`tile_coords`/`tile_corner_lon_lat`/`zoom_for_km_per_px` :122-147), worker pool + disk cache +
  LRU + budgeted poll. Sole external touch is `data_source::fetch_bytes` (tiles.rs:409). Esri
  attribution strings ride along (:75-81). One refactor in the move commit: the private consts
  `MAX_TEXTURES = 220` / `MAX_WORKERS = 4` (:185-186), the disk-cache directory (today
  `settings::tile_cache_dir()`), and the debug env-var name (`BOWECHO_TILE_DEBUG`, :286/:327)
  become a `TileLayerConfig { cache_dir, max_textures, max_workers, debug_env }` constructor
  parameter. BowEcho passes exactly today's values (zero behavior change); mini passes its own
  namespace dir, the phone/desktop texture budget (§7), and `MINIDERECHO_TILE_DEBUG`. The
  hardcoded 96-deep queue cap (:309) stays as-is.
- **`geo.rs`** — `aeqd_forward_km` (main.rs:4007-region symbol; grep it) + `aeqd_inverse_km` +
  their `aeqd_tests` module lifted as free pure functions. Both apps must project identically;
  tiles.rs already assumes this projection for quad corners. BowEcho re-imports in the same commit.
- **Future tenants (post-CP-1):** `render_service.rs` (LaneId, RenderService, the coalescing
  pools) at v0.29 Phase 4a/4b; `loop_engine.rs` (FrameHistory + generation newtype VERBATIM,
  FrameIdentity, FeedSource, LoopEngine, HistoryLimits with byte_budget) at Phase 4c — `pub` types,
  app_ui first consumer, mini second.

### `crates/basemap` — static vector data + pure draw helpers (NEW, M4)

- `basemap_data.rs` (273,005 lines of `&'static [BasemapLine]`/`BasemapLabel` consts — the file
  owns its `pub` types, no main.rs coupling) and `basemap_towns.rs` (32,618 lines) moved verbatim.
  Own crate = the 300k-line const tables compile once and cache; today they recompile with every
  app_ui touch (a build-time win for BowEcho too).
- Draw helpers refactored out of `draw_basemap`/`draw_basemap_overlay`/`draw_basemap_lines` + the
  label rankers (main.rs `&self` methods — grep `fn draw_basemap`) into free functions taking
  `(painter, rect, project: impl Fn(f64, f64) -> Pos2, style)`. BowEcho's methods become thin
  wrappers in the same commit (mechanical).

### `crates/hazards` — warning model + parse, pure (NEW, M3; no egui, no fetch)

The challenge verified the entire chain is free functions over pure data — zero ViewerApp state.
Lift-with-tests (this is the one 1.5-3k-line-class extraction; see the duplication-window risk, §12.5):

- Model: `HazardRecord`/`HazardPoint` (main.rs:4585-4617 — strings, f32s, `Vec<HazardPoint>`, bbox).
- Parse: the CAP/GeoJSON serde types; `parse_weather_alert_feature_with_zones` (:47555) as the
  public entry (empty zone map ⇒ own-geometry polygons only — core warnings carry their own
  polygons; `weather_alert_feature_rings` :47671 uses `feature.geometry` first, zone geometries are
  only the watch/advisory fallback); `parse_weather_alert_tags` (:47825); `parse_vtec_alert`
  (:47979); `parse_warning_vtec_line` (:48174); `parse_warning_tags` (:48256);
  `find_warning_headline` (:48307); helper tail (`weather_alert_family`, `hazard_lifecycle_status`,
  `parse_alert_time`, `hazard_bbox`, `canonical_damage_threat`, labels, `HAZARD_FILTER_FAMILIES`).
- Lifecycle/ordering: `hazard_record_is_active_or_pending` (:43039 region),
  `sort_hazard_records` (:46649), `hazard_record_threat_priority` (:46734),
  `dedupe_hazard_records` (:46879).
- API shape: `parse_active_alerts(geojson: &str, now: DateTime<Utc>) -> AlertLoad` — **fetch stays
  in the caller** (`data_source::fetch_text(ACTIVE_ALERTS_URL)`; the URL const main.rs:584 is
  duplicated into mini, it is one string). SPC-MD enrichment and the zone-geometry network fetch
  (`fetch_weather_alert_zone_geometries` :46517-region) are BowEcho-side extras mini v1 skips.
- Existing parser tests move with it (`hazard_parser_extracts_warning_polygon_and_tags` :65769
  family; default-filter policy test `default_hazard_filters_keep_only_core_warning_families_visible`
  :63643 informs mini's default).
- **Duplication window (accepted, tracked):** BowEcho keeps its main.rs copy until a scheduled
  v0.29-Phase-5-adjacent chore swaps it onto the crate; a deletion ticket is filed when the crate
  lands. The alternative — blocking mini on a 3k-line main.rs surgery mid-v0.29 — is worse.

### `crates/mini_ui` — bin `miniderecho` (NEW, M0 onward)

Own build.rs + winresource, mirroring app_ui/build.rs's env-var pattern (docs/app-identity.md
"Build-time only") with `MINIDERECHO_*` variables and mini defaults for name/icon/VERSIONINFO.

- Deps: eframe (per-target, B1), chrono, serde/serde_json, image, and workspace crates:
  `ui_core`, `data_source`, `nexrad_io`, `radar_core`, `render2d`, `color_tables`, `settings`,
  `cache`, `styles` (defaults only, no editor), `product_engine` (M2+), `basemap` (M4+),
  `hazards` (M3+).
- **Explicit non-deps (the iOS RAM firewall):** none of the 12 rusty-weather git crates
  (rw-ingest, rw-sat, rw-glm, rw-store, rw-ui, rustwx-*, sharprs — crates/app_ui/Cargo.toml:57+);
  no `app_ui` (its lib stub would pull the whole rw-* tree — cargo deps are per-crate, not
  per-target); no rfd (v1 has no file dialogs — zero-config is a feature); no windows-sys/sha2
  updater stack (desktop self-update is a later `cfg(not(target_os = "ios"))` addition).
- **CI firewall gate:** a CI step (or unit test shelling `cargo tree -p mini_ui -e normal`)
  asserting the output contains no `rw-`, `rustwx-`, `sharprs`, or `app_ui` token.
- Module map (target ~4-6k lines + ~1.5-2k test lines — the UX design's honest number; the
  engineering design's 2,800 undercounted theme/readout/form-factor scaffolding):

```
mini_ui/src/
  main.rs          eframe boot, MiniApp::update, module wiring            (~300)
  form_factor.rs   FormFactor::{Phone,Desktop} detect + persisted
                   override + SafeInsets provider (+ debug notch sim)     (~120)
  theme.rs         dark-first egui Style from mini brand tokens;
                   mini's ui-constants module (BAR_H=56, ROW_H=44/28…)    (~250)
  map_view.rs      viewport state, pure MapIntent gesture classifier,
                   painter stack: tiles → vector basemap → radar quad →
                   warnings → chrome; long-press/hover gate readout       (~800)
  feed.rs          MiniFeed: live poll WorkerSlot + backfill StreamSlot
                   + dedupe key + pure liveness()                         (~450)
  frame_ring.rs    FrameHistory-lite + BYTE BUDGET + FrameCursor +
                   live-edge detach/reattach state machine                (~350)
  render_worker.rs ONE coalescing render lane over render2d public API   (~300)
  products.rs      curated product enum + capability-as-value            (~150)
  warnings.rs      slot + draw + fat-finger hit-test + detail card
                   over the hazards crate                                 (~450)
  bar.rs/sheet.rs/scrubber.rs  the 5-control bar, bottom sheet with
                   detents, loop scrubber                                 (~700)
  desktop.rs       top bar + right gear panel + keyboard map              (~350)
  site_picker.rs   search + nearest list over data_source::sites          (~250)
  settings.rs      MiniSettings (Eq-derivable, own config path per B4)    (~200)
  about.rs         credits per policy + "BowEcho is the power app"        (~120)
  fonts.rs         wordmark font embed, fonts.rs conventions              (~80)
```

### iOS boundary

Everything below mini_ui is pure Rust and already compiles for arm64 Apple (ios-port-notes.md:
bzip2-sys and ring are the only C/asm deps; no OpenSSL; reqwest is rustls). v1 network surface is
all-https (S3, api.weather.gov, ipapi-class geoloc endpoint, server.arcgisonline.com) — **no ATS
exceptions needed** (the http GR2A/placefile feeds are BowEcho features mini doesn't have).
mini_ui carries at most three `cfg(target_os = "ios")` sites: (1) settings/data dirs (already
centralized, B4); (2) the eframe backend selection (B1); (3) lifecycle hooks — suspend cancels
slots, which is literally `slot.cancel()` (the drop-receiver contract). **Touch input is NOT
cfg'd:** the phone layout is form-factor-detected, so the identical phone UI is exercised on
desktop by resizing the window — testable in CI, and the dev loop for 100% of the phone UX runs on
Windows today. The later `crates/mini_ios` staticlib (C-ABI start fn + thin Xcode wrapper,
ios-port-notes.md "Build sketch") supplies SafeInsets and CoreLocation; mini_ui never references
UIKit. The B5 macOS-runner check job is the tripwire long before the Mac session.

---

## 3. The v1 radar pipeline (site select → live → loop → pixels)

**v1 live scope decision: US-only** (`SiteKind::Wsr88d | Tdwr | Research`). The realtime-chunk path
(`latest_realtime_level2_volume` / `download_realtime_volume`) is US-only; intl live/loop goes
through the separate `IntlProvider` latest/recent_source/FramePlan family and would roughly double
feed.rs. The exhaustive `SiteKind` match (deliberately NOT `#[non_exhaustive]`, sites.rs:70-83) is
the compile-pressure lever: when intl lands (v1.x), every match site lights up. The site picker
still *lists* only US sites in v1 — honest scope, not a greyed graveyard.

1. **Site model:** `data_source::sites` — `sites_near(lat, lon, radius_km)` (:176) + `all_sites()`
   (:170) drive the picker; selection persists as `SiteRef::settings_key()` (:110) in MiniSettings.
   Mini is the second consumer proving the Phase-1C API.
2. **First-run chain** (pure, tested fallback function of `(Option<last_used>, Option<geoloc>,
   Option<locale>)`): last-used site → IP geolocation → locale-country heuristic → KTLX.
   **Decision (the panel split):** IP geolocation IS in v1 — zero-config nearest-radar is the core
   product promise — with the UX design's mitigations: one https fetch on a WorkerSlot behind a
   trait (testable), 2 s timeout, silent fallback down the chain, endpoint named in About's privacy
   text, and a MiniSettings kill-switch. CoreLocation replaces it on iOS where a permission prompt
   is idiomatic. No dialogs; while the first volume loads, the map shows the dark basemap + site
   marker + honest status text ("KTLX — fetching latest volume…").
3. **Warm launch < 1 s:** `newest_cached_level2_path` (data_source/lib.rs:1145) paints the newest
   disk-cached volume immediately; the live poll then refreshes. Mini caches volumes from day one
   to earn this.
4. **Live poll:** one `WorkerSlot` tick (1 s cadence, the US chunk chain) calls
   `latest_realtime_level2_volume(site)` (lib.rs:777 — listing TTL/memoization/rollover live in the
   crate); dedupe key = `(site, volume_id, chunks.len())` (the `poll_last_file` contract); on new
   data, `download_realtime_volume(&vol, cache_dir)` (lib.rs:982) → decode.
5. **Progressive first paint:** `decode_volume_from_bytes_with_bzip_preview` (nexrad_io/lib.rs:371)
   emits the first displayable cut before the volume completes — a `StreamSlot<DecodeMsg>` carries
   `Preview(Arc<RadarVolume>)` then terminal `Full(..)`/`Failed(..)`. This is how "radar within
   seconds" is honest. Cold first run p50 < 5 s on broadband; the perf-branch audit measured ~39 ms
   decode-to-first-pixels.
6. **FrameRing** (`frame_ring.rs`): mini's ~350-line placeholder for LoopEngine —
   `Vec<Frame { time, site: SiteRef, volume: Arc<RadarVolume> }>` + cursor + playing flag +
   **byte budget enforced from day one** (per-entry cost from MomentGrid storage — U8/U16 grids,
   radar_core/lib.rs:419-region; evict oldest-beyond-budget, cursor stays stable). Install policy =
   the v0.29 spec's `FollowNewestUnlessPlaying`. Loop backfill on demand:
   `recent_level2_objects(site, days_back, max)` (lib.rs:695) → `download_object(LEVEL2_ARCHIVE_BUCKET, ..)`
   (lib.rs:1114) streamed oldest-first through a `StreamSlot`. Deleted when LoopEngine lands (§10).
7. **Render:** ONE background coalescing lane (`render_worker.rs`) — the newest-request-replaces-
   queued pattern from `spawn_render_worker_with_mode` (main.rs:5339) + `merge_render_request`
   (:12738), rewritten small over render2d's PUBLIC API only (`render_viewport_payload` :12766 is
   inseparable from ViewerApp types — pattern-lift, not code-lift): `render_moment_viewport_rgba_into`
   (render2d/lib.rs:326) with one `ViewportMomentCache`/`ViewportSampleCache` (:337/:347), the SRV
   family, `new_dealiased_velocity_with_color_tables` (dealias family :659+), one recycle buffer.
   `ViewportRasterOptions { width, height, radar_x_px, radar_y_px, km_per_px_x, km_per_px_y,
   rotation_rad }` (:99-111) means the raster IS screen-space: drawing is a single textured quad,
   no mesh warp. Deleted when RenderService lands (§10).
8. **Map view:** pan = drag through the projection inverse; zoom = scroll/pinch
   (`InputState::zoom_delta`/`multi_touch` — native egui, verified present in 0.34.3 and proven on
   the owner's iPhone). During gestures the existing texture is transformed cheaply
   (translate/scale the quad — the render-staleness pattern at main.rs:4002-region) while a
   coalesced re-render request streams to the worker; newest-wins does the throttling. Under the
   radar: `ui_core::tiles::TileLayer` rasters or `basemap` vector lines (dark vector default — no
   tile fetch on first paint), both projected through `ui_core::geo::aeqd_forward_km`.
9. **Products/tilts:** v1 product enum = `Ref | Vel | DealiasedVel | Srv | Cc | Zdr | Kdp | Sw` —
   a fresh, small mirror of the render2d-facing subset (BowEcho's `DisplayProduct` :5587 is
   entangled with the DerivedProduct picker machinery). Tilt list derives from `volume.cuts` with
   real elevation angles. M2 adds `Cref | Vil` via product_engine/render2d::volumetric
   (`composite_reflectivity_grid`, `vil_grid`). Unavailable products grey WITH the reason string
   (capability-as-value).
10. **Warnings:** §5.

---

## 4. The responsive shell

**Where the switch lives:** computed every frame in `MiniApp::update` from `ctx.screen_rect()` —
`FormFactor::Phone` when `min(w, h) < 520.0` points, else `Desktop`; persisted override
`Auto | Phone | Desktop` in MiniSettings. iPhone 16 Pro (402×874 pt) → Phone; iPads/desktops →
Desktop. Touch capability (`ctx.input(|i| i.any_touches())`) only fattens hit targets and swaps
hover-verbs for tap-verbs — it never flips the layout (a touchscreen laptop must not reflow).
Killer dev loop: drag the desktop window narrow and the phone layout appears live. Safe areas via a
`SafeInsets` provider: zeros on desktop, UIKit values from the iOS wrapper later, plus a debug
setting that simulates a notch on desktop. Both layouts drive the same `MiniState` through one
`MiniAction` enum dispatched centrally (the `UnifiedPlayerAction` pattern, unified_player.rs:73,
that v0.29 §5 endorses) — behavior testable without pixels.

**PHONE** (bottom sheet + gestures + ≥44 pt targets):

```
┌──────────────────────────────────┐
│ ▁▁▁▁ safe-area top inset ▁▁▁▁▁▁ │
│ ◉ LIVE  KTLX · Reflectivity 0.5°│  ← status strip: liveness chip, site,
│                        21:56:32Z │    product·tilt, DATA time (never wall clock)
│                                  │
│            MAP (full-bleed)      │
│                                  │
│    drag = pan     pinch = zoom   │
│    double-tap = zoom in          │
│    two-finger tap = zoom out     │
│    long-press = inspect card     │
│    tap polygon = warning card    │
│                                  │
│ ▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂▂ │  ← colorbar: 5 pt gradient strip (tap = legend)
│ ◀ │●●●●●●●○│ ▶        21:52Z    │  ← scrubber (visible when history > 1)
│┌──────┬──────┬──────┬─────┬────┐│
││ SITE │ PROD │ TILT │ ▶/◉ │ ≡  ││  ← THE BAR: 5 targets, 56 pt tall
│└──────┴──────┴──────┴─────┴────┘│
│  ▔▔▔▔ home-indicator inset ▔▔▔ │
└──────────────────────────────────┘
```

The **bottom sheet** (opened by SITE/PROD/TILT/≡; three detents: peek 25% / half / full; 180 ms
ease-out via `ctx.animate_value_with_time`; drag-dismiss) contains, sectioned top-to-bottom: site
search + nearest list (distance + beam height from `sites_near`), product grid (greyed tiles carry
the reason string), tilt list, loop length/speed, Layers (v1: exactly "Radar" and "US Warnings"
rows with opacity — the ONE v2 extensibility seam), basemap style (DarkVector default / Satellite /
Streets / Topo), Settings, About. One sheet, never stacked windows.

Gesture map (single-finger arrives as pointer events; pinch via `zoom_delta`/`multi_touch`):
drag=pan; pinch=zoom-to-centroid; double-tap=zoom step in; two-finger tap=zoom step out (RadarScope
convention); long-press 400 ms=inspect readout card (gate value, lat/lon, beam height — sampled
from the decoded cut, ~100 pure lines). **Two-finger vertical swipe = tilt up/down** ships v1.5
behind a pure, unit-tested classifier (after 8 pt of two-finger motion: |log zoom| dominant → pinch,
else translation.y dominant → tilt) with a settings kill-switch; pinch/drag/double-tap alone are a
complete v1.0 set. No camera tilt — this is a 2D app.

**DESKTOP** (thin persistent controls, no sheets):

```
┌──────────────────────────────────────────────────────────────────────┐
│ ◉ miniDerecho [KTLX Oklahoma City ▾][Reflectivity ▾][0.5° ▾] ◉ LIVE 21:56:32Z  ⚙ │ ← one 32 px top bar
├──────────────────────────────────────────────────────────────────────┤
│                                                                      │
│                            MAP                                       │
│                                            ┌───────────────────────┐ │
│   wheel = zoom-to-cursor                   │ ⚠ Tornado Warning     │ │
│   drag = pan                               │ KTLX · exp 22:15Z     │ │ ← click polygon:
│   hover = gate readout (statusline)        │ radar-indicated       │ │   floating card
│   right-click = nearest radars             │ hail 1.0" wind 60mph  │ │
│                                            │ [full text ▾]         │ │
│                                            └───────────────────────┘ │
│ ▂▂▂▂▂▂▂▂▂▂ colorbar ▂▂▂▂▂▂▂▂▂▂                                     │
├──────────────────────────────────────────────────────────────────────┤
│ ▶ ‖  ◀ ▬▬▬▬●▬▬▬ ▶   21:32–21:56Z · 8 frames · 1× ▾     [GO LIVE]   │ ← scrubber strip
└──────────────────────────────────────────────────────────────────────┘
```

Same five verbs as the phone bar, rendered as combos in the top bar. The gear opens ONE narrow
right panel (site/product/tilt duplicated, loop prefs, layers, basemap, settings, about) — one
panel, not BowEcho's tabbed rail. Keyboard: space=play/pause, ←/→=step, L=go live, ↑/↓=tilt,
W=warnings toggle, F=find site. Every glyph button carries `on_hover_text` (house rule).

**The bar — 5 always-visible controls, and everything that is not:** SITE, PROD, TILT, ▶/◉
(loop/live, §6), ≡. Warnings are NOT a bar control — they are always drawn; the map is the
warnings UI. Deliberately invisible in v1 (the whole point): color-table editor, placefiles, data
packs, panes/multi-view, radar overlays/mosaic, archive browser + date pickers, event explorer,
annotations, cross-sections, Vol3D/RHI, satellite/model/GLM/SPC/obs/MPING layers, storm tracks,
camera follows, recording/media, docking, custom-URL polling, FARM. Nothing has a second-level
menu deeper than the sheet. About carries the pressure valve: "Need more? BowEcho is the power app."

**Beauty budget (what "premium" means in egui, concretely):** dark-first, one palette — theme.rs
derives the whole egui Style from mini's own brand-token values through the settings BrandPalette
machinery (settings/src/brand.rs); radar colors from color_tables defaults (Analyst Velocity HD,
color_tables/lib.rs:970; house REF table); hazard/range-ring/age colors from styles-registry
defaults — zero new color systems. Chrome is translucent panels over the map (fill alpha ~235),
hairline 1-physical-pixel separators, 8 pt spacing grid, one ui-constants module. Motion is for
CHROME ONLY, ≤200 ms ease-out; radar data NEVER animates (no crossfades between frames — hard cuts
read meteorologically; no spinners over the map; loading is text truth in the status strip). No
layout shift (chips reserve width), LINEAR texture filtering, colorbar as a 5 pt gradient strip
with units.

---

## 5. Warnings v1

- **Fetch:** one WorkerSlot, 60 s cadence while live, paused when backgrounded;
  `data_source::fetch_text("https://api.weather.gov/alerts/active?status=actual")`
  (ACTIVE_ALERTS_URL, main.rs:584). Parse via the `hazards` crate (`parse_active_alerts`).
- **Display:** default family filter = core warning families only (tornado, severe-thunderstorm,
  flash-flood, special-marine, snow-squall — the same policy BowEcho's test at main.rs:63643 pins),
  colored by styles-crate hazard tokens with escalation subkeys; PDS/emergency = heavier stroke,
  no pulsing. Watches/MDs exist in the parser but ship OFF (later sheet toggles, not new chrome).
  Core warnings carry their own polygons; the zone-geometry fetch is not in v1 (§2 hazards).
- **Tap-for-details:** phone → warning card as a sheet-peek (swipe up for full text); desktop →
  floating card. Card fields map 1:1 onto `HazardRecord` (:4585): headline, office, expiry
  countdown from `valid_end` colored by remaining time, hail/wind/tornado/damage-threat tag chips,
  motion, full text behind a disclosure. Hit-testing is fat-fingered (12 pt tolerance to nearest
  edge); overlapping polygons produce a chooser list, never a guess.
- **Notification posture: NONE in v1.** No background process, no push, no sounds. The card
  reserves a slot row where a v2 "Notify me" toggle would sit — alerting adds a control, not a
  redesign.
- On a non-US site (post-v1 intl): the Layers row reads the capability value honestly
  ("US NWS warnings — none for SMHI sites").

---

## 6. Loop UX and the live-edge state machine

- **Model:** one `MiniFeed` per app. Live poll on the source cadence; backfill = last N volumes on
  a StreamSlot, oldest-first. Install policy `FollowNewestUnlessPlaying`. Playback advances only
  onto rendered frames (farm_live.rs:1486-1495 taught: holding beats jittering over holes).
- **Scrubber:** appears when history > 1. Frame dots + drag playhead + data-time label; ◀/▶ step;
  play/pause. Default dwell 350 ms/frame, end-of-loop hold 700 ms; speeds 0.5×/1×/2×. Default loop
  length 8 frames (~35 min of VCP12); max bounded by the BYTE budget, never a frame-count cap.
- **Live-edge (pure state machine, truth-table tested):** playing at the newest frame = LIVE —
  chip solid green; new volumes append and the window slides. Scrubbing/stepping back **detaches**:
  chip becomes a grey "−12 min ⤴" pill; polling continues, frames append, cursor holds. Tapping
  the pill / GO LIVE = snap to newest, resume follow. Staleness: chip amber past 2× expected volume
  cadence, showing the age ("4 m 10 s old") — always data time. `liveness()` is derived ONLY from
  feed-own state (the v0.29 §3 shape) — the R8 bug class is unrepresentable in mini from day one.
- The ▶/◉ bar button unifies this: ◉ LIVE when following, ▶/‖ when detached; long-press (phone) /
  right-click (desktop) jumps live.
- The scrubber reserves a hidden left-edge calendar affordance where v2 archive time-travel plugs
  in (via ArchiveFrames + the shared archive browser once v0.29 Phase 2 lands — mini never builds
  listing UI).

---

## 7. RAM / perf budget (the iOS ceiling shapes every cache)

Two built-in profiles selected with form factor. MomentGrids are U8/U16 (radar_core), so a
super-res 88D volume decodes to ~40-120 MB depending on VCP/moments — the measurement gate below
replaces guesses with numbers before defaults freeze.

| Store | Phone profile | Desktop profile | Enforcement |
|---|---|---|---|
| Frame history (decoded volumes) | 256 MB (~4-8 volumes) | 1 GB | FrameRing byte budget; evict oldest; per-entry cost from MomentGrid sizes; cursor stable under trim (tested) |
| Loop render cache (RGBA rasters) | 64 MB | thread-scaled 96-512 MB (BowEcho's constants at main.rs:108-110 are the calibration) | LRU |
| Basemap tiles (GPU textures) | 96 (~25 MB) | 220 (today's MAX_TEXTURES) | `TileLayerConfig` (§2) |
| Decode transient | one volume in flight (~40-80 MB peak) | same | WorkerSlot ≤1 in-flight guarantee |
| Render internals | one ViewportMomentCache + SampleCache + recycle buffer | same | single lane |
| Internal raster resolution | capped ≈1.5× points (halves upload, doubles cache depth) | native | `km_per_px_x/y` free parameters |
| **Steady-state RSS target** | **< 450 MB** | < 1.8 GB | measured at M0 and M2 gates |
| HRRR-class ingest (~4.4 GB) | structurally unreachable | structurally unreachable | dependency-firewall CI gate (§2) |

**Latency:** warm launch → pixels < 1 s (disk cache); cold first run → first pixels p50 < 5 s on
broadband (S3 list ~300 ms + partial-volume bzip preview + ~40 ms raster); poll cadence 1 s (chunk
chain); gesture → re-render coalesced, never queued behind stale work.

**Measurement gate (do NOT ship v1 without it):** real per-volume bytes and steady-state RSS on the
KEAX 2026-06-09 derecho fixture (dense super-res), phone profile, 8-frame loop, product-switching
across the loop. If product-switch across a full loop demands volumes the budget can't hold, the
designed escape hatch is: older frames hold rendered RGBA only / drop non-displayed moments —
decide from the M2 measurement, before UI polish bakes assumptions in.

---

## 8. Reuse map (current → mini home)

Anchors verified at 89db485 (challenge-corrected). Grep the symbol at implementation time.

### Consumed AS-IS (crate deps, no changes)

| Piece | Anchor | Notes |
|---|---|---|
| `SiteRef`/`SiteKind`/`SiteRecord`, `settings_key`/`parse_settings_key`, `resolve`/`all_sites`/`sites_near` | data_source/src/sites.rs:56-187 | mini = second consumer proving the Phase-1C API; network-free, UI-thread safe |
| `latest_realtime_level2_volume(_with_listing_ttl)` | data_source/lib.rs:777/:787 | takes `&str` US id; TTL memo + rollover re-list inside |
| `download_realtime_volume` / `download_object` / `recent_level2_objects` / `newest_cached_level2_path` | :982 / :1114 / :695 / :1145 | live download, backfill, warm launch |
| `fetch_text` / `fetch_bytes` | :321 / :408 | alerts + geoloc + tiles |
| `decode_volume_from_path` / `decode_supported_volume_bytes` / `decode_volume_from_bytes_with_bzip_preview` | nexrad_io/lib.rs:99/:192/:371 | preview = progressive first paint |
| `RadarVolume`/`ElevationCut`/`MomentType`/`MomentGrid` | radar_core/lib.rs | the decode↔render currency; U8/U16 grids |
| `ViewportRasterOptions` (km_per_px_x/y!) / `render_moment_viewport_rgba_into` / Viewport*Cache / SRV + dealias families | render2d/lib.rs:99/:326/:337-358/:659+ | raster IS screen-space; single quad draw |
| product registry / `composite_reflectivity_grid` / `vil_grid` | product_engine/lib.rs:47-121; render2d::volumetric | M2: CREF/VIL |
| `analyst_hd_velocity_table` + house tables | color_tables/lib.rs:970 | defaults only |
| `set_storage_namespace`/`storage_root_for_namespace` + NEW `config_path_for_namespace` | settings/lib.rs:840-865 (+B4 helper) | namespace `"miniderecho"`; never AppSettings::load/save |
| styles registry defaults (hazard/range-ring/radar-age tokens) | crates/styles | defaults only, no editor UI |

### Moved into shared crates (one copy, both apps)

| Piece | From | To | Diff allowed |
|---|---|---|---|
| WorkerSlot/StreamSlot/WorkerTx | app_ui/src/worker_slot.rs (456 L) | ui_core::worker_slot | pub(crate)→pub; B3 test-util gate; neutral doc comments |
| TileLayer/TileStyle/tile math/pool | app_ui/src/tiles.rs (489 L) | ui_core::tiles | `TileLayerConfig` (cache_dir, max_textures, max_workers, debug_env) |
| `aeqd_forward_km`/`aeqd_inverse_km` + tests | main.rs (grep symbols) | ui_core::geo | none (free fns) |
| basemap_data.rs (273,005 L) + basemap_towns.rs (32,618 L) | app_ui | basemap crate | none (owns its pub types) |
| draw_basemap family + label rankers | main.rs `&self` methods | basemap free fns w/ projection closure | de-methodization; BowEcho wrappers same commit |
| HazardRecord/HazardPoint + parse/VTEC/tags/lifecycle/sort/priority/dedupe + tests | main.rs:4585/:46512/:46649/:46734/:46879/:47555/:47825/:47979/:48174/:48256/:48307/:65769+ | hazards crate | fetch excluded; `_with_zones` is the public shape (empty zone map in v1); BowEcho keeps its copy w/ deletion ticket |

### Pattern-lifted, rewritten small in mini (temporary custody; deletion tickets tied to v0.29)

| Pattern | Studied at | Mini home | Replaced by |
|---|---|---|---|
| Coalescing render worker (newest-wins per lane, recycle buffers) | `spawn_render_worker_with_mode` :5339, `merge_render_request` :12738, `render_viewport_payload` :12766 (inseparable from ViewerApp types) | render_worker.rs (~300 L over render2d public API) | ui_core::RenderService (v0.29 4a/4b, post-CP-1) |
| FrameHistory + byte budget | `FrameHistoryEntry` :2631-region, generation stamp; spec §3 HistoryLimits.byte_budget (mini ships it FIRST) | frame_ring.rs | ui_core::LoopEngine (Phase 4c) — mini's stepper/install tests transfer as acceptance tests |
| Poll dedupe + cadence | `poll_last_file` contract, `install_polled_volume` :31088-region | feed.rs on WorkerSlot | `FeedSource::Live(SiteRef)` + `LoopEngine::poll_cadence` |
| liveness/mode chip | `mode_chip_state` :36644 family | pure `liveness()` (spec §3 shape), truth-table tested | LoopEngine::liveness |
| Map input (drag dead-zone pan via projection inverse, zoom-to-cursor) | main.rs:26767-region | pure MapIntent classifier in map_view.rs | — (mini-owned) |
| Site picker/nearest ranking | `best_radar_candidates` :28385 + BeamCandidate beam heights | thin rewrite over `sites_near()` | — (BowEcho's is welded to Vec<RadarSite> indices Phase 3 deletes) |
| Player verbs | unified_player.rs `UnifiedPlayerAction` :73 | MiniAction enum | — (pattern only) |

### Studied, NOT lifted

`farm_live.rs` (1,805 L — the self-contained-viewer existence proof and sizing calibration; its
core poll→fetch→playback→draw is ~850 L, but it displays pre-rendered PNGs, NOT an L2
decode→render pipeline — it calibrates shell/loop mechanics only). BowEcho's warning UI, unified
player window, layers rail, dock — desktop-dense; mini's shell is fresh by design.

---

## 9. DO-NOT-TOUCH

Moved-not-modified is allowed only where §8 says so; **no behavior edits** to:

- Everything in v029-engine-spec.md §9 — mini's extractions must not weaken any of it.
- **Decode paths:** all of nexrad_io (odim.rs, jma.rs, the magic-byte router, the preview decode).
- **render2d internals:** rasterization, caches, dealiasers, SRV math, volumetric products.
- **The US realtime chunk chain** (barrier-free downloads, listing TTL/rollover, live-partial
  guards) — mini consumes it as-is; no `LiveService` unification (explicitly out, per v0.29).
- **worker_slot semantics** in the move: drop-rx cancellation, send+repaint, poll-never-blocks,
  Ready/Disconnected-clears-rx — visibility and doc wording are the only diffs (§2).
- **tiles.rs anti-shear machinery and the AEQD rotation fix** — config injection only.
- **BowEcho's behavior in every extraction commit** — its tests are the gate; status strings and
  env-var values it passes stay byte-identical (`BOWECHO_TILE_DEBUG` keeps working).
- **app_ui/src/main.rs beyond `mod`/`use` lines and the thin wrappers §8 names** — mini work never
  refactors BowEcho logic opportunistically; that is the v0.29 fleet's file.
- **The settings/styles document locations for BowEcho** (B4 is additive; `bowecho_config_dir`
  semantics unchanged).
- **The credit policy** — mini's About may only extend, never trim it.

---

## 10. v0.29 adoption points (mini is the second consumer that proves each API)

| v0.29 artifact | Status | Mini adoption | What mini deletes |
|---|---|---|---|
| `SiteRef`/sites.rs (Phase 1C) | **landed at HEAD** | NOW — picker, settings key, feed identity | — (born on it) |
| `WorkerSlot`/`StreamSlot` (Phase 1B) | landed at HEAD | NOW via the ui_core move (M0) | — (born on it) |
| ArchiveFrames + shared archive browser (Phase 2) | pending | v2 archive time-travel behind the scrubber's reserved calendar affordance | nothing (mini never builds listing UI) |
| RenderService in ui_core (Phase 4a/4b, **post-CP-1**) | pending | swap render_worker.rs body for a lane on the shared service | render_worker.rs (~300 L) |
| LoopEngine + FrameHistory + HistoryLimits.byte_budget in ui_core (Phase 4c, **post-CP-1**) | pending | `MiniFeed`/`FrameRing` become a `LoopEngine` instance with `EngineRole`-equivalent policy; mini's stepper/install/liveness tests transfer as acceptance tests | frame_ring.rs (~350 L) + feed.rs internals |
| Two-button LIVE|ARCHIVE bar (Phase 5) | pending | none — mini's ▶/◉ + GO LIVE is already that posture | — |

Mini's placeholder types are deliberately shaped on the spec's names (`FeedSource`,
`SelectionPolicy::FollowNewestUnlessPlaying`, `liveness()`, `HistoryLimits.byte_budget`) so each
swap is mechanical. If mini ships v1 before Phase 4c, the LoopEngine swap is the first post-v1
task, not optional (the fourth-loop-engine drift risk, §12.2).

---

## 11. Milestones and gates

**Every gate, uniformly:** `cargo test --workspace` green (never below the branch-point count;
growing each milestone) · `cargo fmt --check` + `cargo clippy --workspace -D warnings` clean ·
BowEcho behavior unchanged in extraction commits (its suite is the proof) · the dependency-firewall
CI gate green from M0 on · owner runs a named checkpoint on a locally built release-fast exe.
Extraction commits and behavior commits never mix.

- **M0 — walking skeleton** (§13 work order). Gate: skeleton acceptance list; BowEcho green on
  ui_core; first-pixels + steady-RSS numbers recorded on real hardware.
- **M1 — products, tilts, site picker, settings, status strip.** Product enum + capability-as-value
  (tested per product); tilt list from volume.cuts; site picker over sites_near/all_sites;
  MiniSettings round-trip tests (Eq-derivable, unknown-key tolerant, B4 path); first-run fallback
  chain (pure fn, tested; geoloc behind trait with 2 s timeout + kill-switch); B5 macOS-runner iOS
  check job lands in CI. Gate: cold first run < 5 s p50 broadband demo; settings round-trips.
- **M2 — loops.** FrameRing byte budget + trim tests (cursor stable); backfill StreamSlot;
  scrubber; live-edge state machine truth table; CREF/VIL via product_engine; **the §7 measurement
  gate on the KEAX 2026-06-09 fixture** — phone-profile RSS recorded, retention escape hatch
  decided. Gate: measurement numbers in the PR; loop feel checkpoint (owner).
- **M3 — warnings + phone layout.** hazards crate extracted (tests ride along; BowEcho copy stays,
  deletion ticket filed); warnings fetch/draw/card/hit-test (chooser on ambiguity, 12 pt tolerance);
  FormFactor + SafeInsets + bottom sheet + bar + gesture classifier (pure, tested incl.
  double-tap/two-finger-tap). Gate: resize-to-phone dev-loop demo covers 100% of phone UX on
  Windows; default-filter test mirrors BowEcho's policy test.
- **M4 — beauty + basemap crate + branding + release engineering.** basemap crate extracted (both
  apps on it — BowEcho build-time win); dark vector default + tile styles; theme.rs token pass;
  About/credits per policy; mini build.rs icon/VERSIONINFO; two-artifact release workflow + signing
  per docs/SIGNING.md (budget real time: the v0.27.1 fat-LTO linker-OOM history in the workspace
  Cargo.toml release-profile comment applies to CI with two bins — mini's bin is far smaller and
  the pagefile fix exists, but verify). Gate: owner first-run checkpoint on a clean machine:
  launch → radar < 5 s, zero dialogs, phone layout by window resize, warning tap, loop scrub,
  GO LIVE.
- **M5 — iOS harness spike (spike, not promise).** `crates/mini_ios` staticlib (C-ABI start fn) +
  thin Xcode wrapper; wgpu backend leg (B1); SafeInsets from UIKit; lifecycle suspend =
  `slot.cancel()` sweep; CoreLocation replaces IP geoloc. Exit criteria: mini runs on the owner's
  iPhone 16 Pro with touch + live loop; RSS measured against the §7 phone column. Anything beyond
  (TestFlight, ATS review, App Store) is post-spike scoping.

---

## 12. Risks and mitigations

1. **CP-1 timing (highest coordination risk).** If the v0.29 fleet lands LoopEngine/RenderService
   app_ui-internal first, mini's placeholders live longer and a second move is needed. Mitigation:
   CP-1 is a doc edit TODAY (§1.B2); M0's ui_core::worker_slot move establishes the mechanics; the
   spec amendment is called out in this doc and must be applied before Phase 4 starts.
2. **Fourth-loop-engine drift.** FrameRing/MiniFeed is by construction another site+history+texture
   machine — the defect class v0.29 §0 kills. Mitigation: shapes pinned to spec names, liveness
   truth table as tests, adoption is a named deletion (§10); if v1 ships before Phase 4c, the swap
   is the first post-v1 task.
3. **Two render paths until RenderService.** Mini's trimmed worker vs BowEcho's — drift class.
   Mitigation: mini's worker touches only render2d public API, keeps newest-wins semantics exactly,
   carries a deletion ticket tied to Phase 4a.
4. **RAM under super-res loops on phone.** 4-8 retained volumes is a thin loop; product-switching
   across 12 frames may exceed budget. Mitigation: byte budget from day one + the M2 measurement
   gate decides the retention escape hatch before polish bakes assumptions in.
5. **hazards duplication window.** VTEC/tag fixes land twice until BowEcho migrates. Mitigation:
   extraction carries tests verbatim; the crate API is a superset of main.rs behavior; BowEcho's
   switch is scheduled as a v0.29 Phase-5-adjacent chore with a filed ticket.
6. **eframe-on-iOS is not turn-key** even with wgpu (winit lifecycle, insets, Metal upload
   profiling unproven here). Mitigation: B1 backend decision is locked and costless; B5 CI
   tripwire; 100% of phone UX testable on Windows; M5 stays a spike. Owner's iPhone run de-risks
   input/rendering, not lifecycle/packaging.
7. **Extraction collisions with the write fleet.** worker_slot/tiles/basemap/hazards moves touch
   main.rs `mod`/`use` lines while v0.29 Phases 2-3 are in flight; anchors drift daily. Mitigation:
   verbatim leaf moves, one crate per commit, landed within days; symbol-anchored work orders;
   re-grep at start. hazards is the only overlap with planned v0.29 work — coordinate so app_ui's
   copy survives until its scheduled swap.
8. **IP geolocation** (privacy + third-party availability). Mitigation: trait-wrapped single https
   fetch, 2 s timeout, silent fallback to locale→KTLX, endpoint named in About, kill-switch;
   last-used site makes it first-run-only. CoreLocation on iOS.
9. **Release engineering is not free.** Two artifacts, two icons/VERSIONINFO, signing paths, the
   fat-LTO CI history. Mitigation: M4 budgets it explicitly; mini can ship `lto = "thin"` if the
   fat-LTO leg regresses CI.
10. **Scope pressure toward BowEcho features.** Every feature is one `use` away. Guardrails: the
    bar is capped at 5 controls by this doc; the sheet's section list is enumerated in §4;
    additions require removing something or landing as a v2 Layers row; the dependency firewall is
    the structural brake; About's "BowEcho is the power app" line is the pressure valve.
11. **Divergent settings worlds.** Mini's namespace means favorites/last-site never collide with
    BowEcho — and never share; users running both pick sites twice. Accepted for v1 (zero-config
    makes it cheap).
12. **Warning hit-testing on outbreak days** (stacked SVRs). Designed answer (chooser list + 12 pt
    tolerance) needs field validation on a real event before v1.0 tags.

**Named non-goals for v1** (scope pressure has an answer): intl live/loops (v1.x — the SiteKind
match is the lever; ORD/SMHI/13 providers already exist in data_source when wanted); archive
browsing (v2, via v0.29 Phase 2); notifications/alerting; placefiles; multi-pane; mosaic; MRMS or
any national composite (a new data product, not v1); satellite/model/GLM layers; recording;
self-update on iOS; any async runtime.

---

## 13. Walking-skeleton work order — M0 (hand to an implementation agent verbatim)

**Branch:** off `v028/unslop` @ HEAD. **Ground rules:** re-grep every line anchor before editing
(they drift; symbols are durable). Extraction commits never mix with behavior commits; when a task
says "verbatim", `git diff` must show a pure move plus only the diffs the task enumerates.
`cargo test --workspace`, `cargo fmt --check`, `cargo clippy --workspace -D warnings` green at
every commit. Do not touch anything in §9. Windows note: never bulk-rewrite UTF-8 sources with
PowerShell pipelines; use targeted edits.

### Task 1 — `crates/ui_core` (extraction; BowEcho must stay green)

1. New crate `crates/ui_core` (lib). Deps: `eframe` (workspace), `image` (workspace),
   `data_source` (path). Feature: `test-util = []`.
2. Move `crates/app_ui/src/worker_slot.rs` → `crates/ui_core/src/worker_slot.rs` VERBATIM
   (456 lines). Allowed diffs, exhaustively: every `pub(crate)` → `pub`; the two
   `inject_for_test` gates become `#[cfg(any(test, feature = "test-util"))]`; doc comments naming
   BowEcho-specific fns (`cancel_extra_pane_load_for_user_command`, `run_self_update_worker`)
   rephrased app-neutrally. Its unit tests move with it.
3. Move `crates/app_ui/src/tiles.rs` → `crates/ui_core/src/tiles.rs` VERBATIM (489 lines), then
   ONE refactor commit: introduce `TileLayerConfig { cache_dir: PathBuf, max_textures: usize,
   max_workers: usize, debug_env: &'static str }` consumed by the constructor; replace the private
   consts `MAX_TEXTURES`/`MAX_WORKERS` (:185-186), the `settings::tile_cache_dir()` call, and the
   `"BOWECHO_TILE_DEBUG"` literals (:286/:327) with config fields. BowEcho passes exactly today's
   values (220, 4, `tile_cache_dir()`, `"BOWECHO_TILE_DEBUG"`) — zero behavior change, its tile
   tests prove it.
4. New `crates/ui_core/src/geo.rs`: move `aeqd_forward_km` + `aeqd_inverse_km` + the `aeqd_tests`
   module out of main.rs as free pure functions (grep the symbols; they are free fns today).
5. app_ui migration commit: add `ui_core` dep (+ dev-dep with `test-util`); delete
   `mod worker_slot;`/`mod tiles;`; rewrite the use-sites (single `use worker_slot::{...}` at
   main.rs:77 for slots; grep `tiles::` and `aeqd_forward_km`/`aeqd_inverse_km` call sites).
   BowEcho suite green = the gate.
6. Add the additive settings helper (separate commit):
   `pub fn config_path_for_namespace(namespace: &str) -> Option<PathBuf>` in
   crates/settings/src/lib.rs beside `storage_root_for_namespace` (:859), with a unit test. Do NOT
   change `bowecho_config_dir`, `tile_cache_dir`, or the data-dir override.

### Task 2 — `crates/mini_ui`, bin `miniderecho`

New bin crate. Cargo.toml: `[[bin]] name = "miniderecho"`; deps `ui_core`, `data_source`,
`nexrad_io`, `radar_core`, `render2d`, `color_tables`, `settings`, `cache`, chrono, serde,
serde_json, image; eframe per-target exactly as §1.B1 (desktop = workspace glow eframe; the iOS
target block may be written now — it is inert on Windows). **Forbidden deps:** `app_ui`, any
`rw-*`/`rustwx-*`/`sharprs`, `rfd`. A plain `build.rs` setting a distinct window/exe name is
enough for M0 (icon/VERSIONINFO is M4).

Boot sequence in `main()`: `settings::set_storage_namespace(Some("miniderecho"))`; data root =
`MINIDERECHO_DATA_DIR` env if set, else `settings::storage_root_for_namespace("miniderecho")`;
volume cache dir = `<data_root>/volumes/`; window title "miniDerecho".

**Hardcoded scope for M0:** site KTLX (`SiteRef::parse_settings_key("KTLX")` — prove the API),
product Reflectivity, lowest tilt, dark background (NO basemap, NO tiles on screen — Task 1's
tiles move is for BowEcho parity and later use). Desktop layout only. No settings persistence, no
site picker, no warnings, no product/tilt UI, no bottom sheet, no theme pass, no about screen.

### Task 3 — feed: poll → download → decode (all on ui_core slots)

1. `feed.rs`: a 1 s-tick `WorkerSlot<PollResult>` job calling
   `data_source::latest_realtime_level2_volume("KTLX")` (lib.rs:777). Dedupe key
   `(site, volume_id, chunks.len())` from `RealtimeLevel2Volume` (lib.rs:142-149) held by the app;
   unchanged key ⇒ no download. On new data the job runs
   `download_realtime_volume(&vol, &cache_dir)` (lib.rs:982) and sends the `DownloadedObject.path`.
2. Decode on a `StreamSlot<DecodeMsg>` where
   `enum DecodeMsg { Preview(Arc<RadarVolume>), Full(Arc<RadarVolume>), Failed(String) }`
   (`is_terminal` = Full|Failed): read bytes, call
   `nexrad_io::decode_volume_from_bytes_with_bzip_preview(&raw, min_radials, |v| tx.send(Preview(..)))`
   (nexrad_io/lib.rs:371), then send Full. Preview installs immediately (progressive first paint);
   Full replaces it by identity.
3. Warm launch: before the first poll, `data_source::newest_cached_level2_path(&cache_dir)`
   (lib.rs:1145) → if Some, decode and install it so pixels appear < 1 s on relaunch.
4. Status truth: a one-line status string in the top strip driven at drain time (never written by
   jobs — the WorkerSlot rule), always showing DATA time of the displayed frame, never wall clock.

### Task 4 — FrameRing with the byte budget (the loop)

`frame_ring.rs`: `struct Frame { time: DateTime<Utc>, site: SiteRef, volume: Arc<RadarVolume> }`;
`struct FrameRing { frames: Vec<Frame>, cursor: usize, playing: bool, byte_budget: usize }`.
Install = upsert by (site, time) identity, sort by time, evict oldest while over budget (per-entry
cost = sum of MomentGrid buffer lengths; write a `volume_bytes(&RadarVolume) -> usize` helper),
cursor policy = `FollowNewestUnlessPlaying` (follow newest when at live edge and not scrub-playing;
hold otherwise). Desktop budget 1 GB for M0.

Backfill: on startup after the first live frame, one `StreamSlot` job:
`recent_level2_objects("KTLX", 1, 8)` (lib.rs:695) →
`download_object(LEVEL2_ARCHIVE_BUCKET, obj, &cache_dir)` (lib.rs:1114, bucket const :22) →
`decode_volume_from_path` (nexrad_io/lib.rs:99) → send oldest-first; install each.

Playback for M0: keyboard only — space = play/pause (350 ms dwell, 700 ms end-hold), ←/→ = step,
L = jump to newest + resume follow. Playback advances only onto frames whose render is available
(hold, don't jitter). No scrubber widget yet.

**Unit tests (required):** byte-budget trim (install N frames of known synthetic sizes → oldest
evicted, cursor stable, order preserved); FollowNewestUnlessPlaying truth table (at-edge/detached ×
playing/paused × install); upsert dedupe by identity.

### Task 5 — the coalescing render worker

`render_worker.rs`: one `std::thread` + two mpsc channels (requests in, results out — this thread
is allowed to own channels; it IS mini's render service placeholder).
`struct RenderReq { volume: Arc<RadarVolume>, cut_index: usize, moment: MomentType, options: ViewportRasterOptions, key: u64 }`.
Queue discipline: newest-replaces-queued (queue depth 1 — an incoming request overwrites any
unstarted one; the pattern of `merge_render_request` main.rs:12738 degenerate to one lane). Worker
body: reuse ONE `ViewportMomentCache` across requests keyed on (volume Arc ptr, cut, moment)
(render2d/lib.rs:337 — it caches exactly that), one recycled `Vec<u8>` pixel buffer sized by
`viewport_rgba_buffer_len` (:114), call `render_moment_viewport_rgba_into` (:326), send
`(key, w, h, pixels)` + `ctx.request_repaint()`. UI drains, uploads a LINEAR-filtered
`ColorImage`/texture, keeps `(key ↔ texture)` so stale results are dropped.

**Unit test:** queue discipline — push 3 requests while the worker is blocked (test hook or a
slow first job), assert only the newest unstarted one renders.

### Task 6 — map view: pan/zoom over the quad

`map_view.rs`: view state `{ center_east_km: f64, center_north_km: f64, km_per_px: f32 }` relative
to the radar (KTLX lat/lon from `sites::resolve`). `rotation_rad = 0.0` for M0.
`ViewportRasterOptions { width, height, radar_x_px, radar_y_px, km_per_px_x: km_per_px,
km_per_px_y: km_per_px, rotation_rad }` derived from view state each time a render is requested.
Input (pure classifier fn `map_intent(&InputState, rect) -> Option<MapIntent>`, unit-tested):
drag → pan (shift center by `-drag_delta * km_per_px`); scroll / `zoom_delta()` → zoom about the
pointer position (clamp km_per_px to [0.05, 20.0]). During a gesture, draw the LAST rendered
texture transformed (translate/scale the quad from the render's options vs current view) and issue
a coalesced render request every frame — newest-wins throttles. Draw order: dark fill → radar quad
→ status strip. A range-ring circle at 100/200 km via `aeqd`-consistent scaling is optional polish,
nothing else.

### Task 7 — CI gates

1. **Dependency firewall:** CI step (or `#[test]` shelling out) running
   `cargo tree -p mini_ui -e normal` and failing on any of `rw-`, `rustwx-`, `sharprs`, `app_ui`.
2. Workspace fmt/clippy/test legs now include the two new crates.
3. (Defer to M1: the B5 macOS `aarch64-apple-ios` check job.)

### Acceptance (the M0 gate — demonstrate all of it)

1. `cargo run -p mini_ui --release` on Windows: window opens dark, and live KTLX reflectivity
   pixels appear — cold < 5 s p50 broadband, relaunch < 1 s (warm cache).
2. Drag-pan and wheel-zoom are 60 fps-smooth with the stale-texture transform; the re-render
   catches up without ever queuing stale work (verify with a log line: requests coalesced).
3. Backfill fills ~8 frames; space plays the loop with data-time in the strip; ←/→ step; L snaps
   to newest and live-follow resumes; a new live volume arriving during pause does NOT move the
   cursor (FollowNewestUnlessPlaying).
4. BowEcho: full workspace suite green; tiles/worker_slot behavior unchanged
   (`BOWECHO_TILE_DEBUG` still works; slot tests pass via the `test-util` feature).
5. Record in the PR description: first-pixels ms (cold + warm), steady-state RSS after a 10-minute
   live session with an 8-frame loop, and per-volume decoded bytes for the KTLX VCP in effect —
   these calibrate the §7 table before M2 freezes defaults.
6. Firewall gate green; fmt/clippy green; every new pure function has tests (dedupe key, byte
   budget, cursor policy, map intent, queue discipline).

### Explicitly NOT in M0 (do not build)

Basemap/tiles on screen (Task 1 moves tiles for BowEcho parity only) · site picker · any product
other than REF · tilt UI · scrubber widget · warnings · bottom sheet/phone layout/FormFactor ·
theme/branding/icon · MiniSettings persistence · geolocation · hazards/basemap crates · intl
anything · archive UI · iOS target work beyond the inert Cargo.toml block.

**Deliverable:** PR series (ui_core extraction; settings helper; mini_ui skeleton), the acceptance
evidence above, and a list of every anchor that had drifted (with corrected lines) for this spec's
next revision.

## 11a. M1 scope amendment (2026-07-02, owner feedback on M0)

The owner's first M0 impression: "literally just the radar and zero
basemap." Correct product signal — a radar app without a map does not
read as an app. M1 therefore pulls forward from M4: **tile basemap on
screen** (ui_core::tiles is already extracted and unused on screen;
draw it under the radar with a miniderecho TileLayerConfig) and the
**always-visible control bar** (the §UX "RadarScope bar": site,
product, tilt, loop transport, GO LIVE). The M4 items that stay at M4:
the 305k-line vector-basemap crate extraction (the tile raster serves
until then), theme/token pass, branding/icon, release engineering.
