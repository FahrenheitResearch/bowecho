# v0.29 Phase 4c — Install-path behavior census

**Spec authority:** `docs/v029-engine-spec.md` §7 (Phase 4c entry gate), §3
(SelectionPolicy), §10 risk 2 (SelectionPolicy rot). This census is the
blocking entry-gate deliverable: it tables every micro-divergence between the
FOUR install paths BEFORE any `LoopEngine` code merges. Per §12a (CP-1) the
engine this census feeds lands `pub` in `crates/ui_core`, not app_ui.

**Provenance:** branch `v028/unslop`, HEAD `9168c27`, censused 2026-07-02.
All line anchors verified at this commit — **re-grep symbol names before
editing; anchors drift** (spec §10 risk 7).

**How to read this:** one section per behavior dimension. Each has a
per-path table, file:line evidence, the tests that pin the behavior, a
**Recommendation** (census author's reasoning only), and a **Decision** row
left blank — keep-as-SelectionPolicy vs consciously-normalize belongs to the
orchestrator and owner, per row, before engine code merges.

---

## 0. The four install paths (anchors at HEAD 9168c27)

| # | Function | Anchor | Role | Fed by |
|---|---|---|---|---|
| P1 | `install_decoded_load_batch` | main.rs:8215 | Primary archive/latest/loop batch install | `poll_async_load` drain (:11200; History at :11221, Final at :11250); workers spawned by latest/archive/event/ORD/SMHI loaders |
| P2 | `install_extra_pane_decoded_load_batch` | main.rs:14409 | Pane batch install | `poll_extra_pane_loads` drain (:14295; History :14321, Final :14352); `follow_primary_volume_into_pane` (:7257, mirrors primary's newest Arc); pane intl loads (`start_extra_pane_intl_load` :14059) |
| P3 | `install_polled_volume` | main.rs:31103 | Primary poll/live install — US custom-URL AND intl share it | `drain_polled_volume` (:31174, one-shot poll tick for `poll_custom_url` :31756 / `poll_intl` :31402); `drain_intl_loop_load` (:31323, streams Load Loop frames one at a time) |
| P4 | `install_radar_layer_volume` | main.rs:7773 | Overlay layer install (single frame) — thin wrapper over `install_radar_layer_history` (:7777, the batch form) | `poll_radar_layer_loads` drain (:7705; Preview :7717, History :7722, Final :7738); `poll_intl_radar_layer_loads` (:7502, intl overlay feed at :7527) |

Support shared by P1/P2/P3: `frame_identity_for_volume` (:51499),
`history_contains_other_site` (:51640), `frame_status_priority` (:51554),
`live_partial_frame_has_new_data` (:51563), `normalized_history_limit`
(:51424), `should_defer_live_partial_selection_for_active_product` (:50003).
P1/P3 display via `install_volume_arc` (:8748); P2 via
`install_extra_pane_volume_arc` (:14642); P4 selects via
`select_radar_layer_history_frame` (:7807).

Spec §3's three SelectionPolicy variants map: P3 =
`FollowNewestUnlessPlaying`, P1/P2 = `SelectAnchor { identity }` (guarded),
P4 under `timeline_sync` = `KeepCursor`. Rows D5–D7 below record where the
real code is finer-grained than those three names.

---

## D1. History mutation model — upsert vs append vs replace-all

| Path | Behavior | Evidence |
|---|---|---|
| P1 | **Upsert by identity** per frame, then sort, then trim | :8274-8279 → `upsert_history_frame` :8363 |
| P2 | **Upsert by identity** per frame (duplicated body), then sort, then trim | :14442-14449 → `extra_pane_upsert_history_frame` :14510 |
| P3 | **Upsert by identity, single frame**, then sort, then trim | :31113-31132 |
| P4 | **REPLACE-ALL**: `layer.frame_history = batch.frames…collect()`, then sort; no upsert, no trim | :7788-7795 |

- P4's live intl feed therefore holds a history of exactly ONE frame per
  poll tick (`DecodedLoadBatch::single` at :7774); overlay loops exist only
  when an archive-window batch replaces the history wholesale
  (`start_radar_layer_archive_window_load` :7582 clears then installs).
- All four sort ascending by `FrameIdentity` (site, scan time): :8277,
  :14446, :31130, :7793-7795. Uniform — no divergence in sort order.

**Pinned by:** `polled_volume_same_site_upserts_and_playing_loop_keeps_its_cursor`
(:60400), `polled_volume_history_stays_time_sorted` (:60361),
`coordinated_one_frame_overlay_never_paints_a_future_final_scan` (:59741).

**Recommendation:** engine `install_batch` = upsert+sort+trim (the P1/P2/P3
shape). P4's replace-all is load-bearing for coordinated loops (comment at
:7589-7593: never keep painting an unrelated previous frame) — model it as
an explicit `clear + install_batch`, not as a fourth upsert mode.
**Decision:** ADOPT the recommendation. Engine `install_batch` = upsert+sort+trim; the overlay replace-all is modeled as explicit `clear + install_batch`, never a fourth upsert mode.

---

## D2. Upsert conflict resolution (same identity already present)

| Path | Behavior | Evidence |
|---|---|---|
| P1 | 3-tier: (a) live-partial with MORE radials replaces (`live_partial_frame_has_new_data`); (b) same path+status → refresh `timings`/`source_label` only; (c) replace only if `frame_status_priority` is higher, or equal priority AND different path | :8371-8381 |
| P2 | **Identical 3-tier logic, duplicated verbatim** | :14528-14538 |
| P3 | **Unconditional replace** — `*existing = frame;` no guards at all | :31121-31126 |
| P4 | n/a (replace-all, D1) | — |

- Status priority ladder (:51554): Preview 0 < LivePartial 1 <
  Complete/Stale 2 < LiveComplete/Local 3. P3 frames are hardcoded
  `FrameStatus::Complete` + `timings: None` + path `poll://{name}` +
  `source_label "polled {name}"` (:31113-31120), so the missing guards are
  currently unreachable-by-construction (a poll never delivers Preview or
  LivePartial) — but the CODE diverges.

**Pinned by:** `live_partial_history_upsert_replaces_same_path_when_radials_increase`
(:65238), `frame_work_cache_key_keeps_live_partial_replacements_distinct`
(:60980), `polled_volume_same_site_upserts_and_playing_loop_keeps_its_cursor`
(:60427 asserts the unconditional in-place replace: repeat identity with new
path wins).

**Recommendation:** one engine upsert with the P1/P2 3-tier rules; P3
callers keep winning because a Complete frame with a new path beats a
Complete frame with an old path under rule (c). Verify with a differential
fixture before normalizing (the poll:// path always differs per tick, so
rule (c) fires — the pinned test at :60427 stays green). This is a
**consciously-normalize** candidate, not a SelectionPolicy.
**Decision:** CONSCIOUSLY NORMALIZE to the P1/P2 3-tier rule, proven by a differential fixture over the polled path before merging (rule (c) must keep every current P3 caller winning).

---

## D3. Cross-site clear rules

| Path | Trigger | What it says | Evidence |
|---|---|---|---|
| P1 | active volume's site ≠ batch anchor site OR `history_contains_other_site` | status = `"history reset (site change to {site}, had {n} frames)"` (deliberate diagnostic, comment :8265-8266) | :8259-8272 |
| P2 | same predicate against the PANE's volume/history | same wording, into `pane.status` | :14426-14440 |
| P3 | same predicate | **SILENT** — no status written | :31105-31112 |
| P4 | n/a — replace-all makes cross-site trivial; separate pre-clears exist at load START (`start_radar_layer_archive_window_load` :7594-7605) | — | — |

Related load-START clears (outside the install fns, but part of the observable
contract): `start_extra_pane_intl_load` clears pane history+display only on
pin change (:14089-14098); ORD/SMHI/intl archive loads clear primary history
before spawning (`start_ord_archive_window_load` :31715,
`archive_browser.rs:1055`); intl poll switch keeps same-site history
(`intl_poll_switch_keeps_active_same_site_history_and_clears_the_rest` :61801).

**Pinned by:** `installing_new_site_batch_drops_previous_site_history`
(:60302), `polled_volume_drops_previous_site_history` (:60335),
`history_scope_detects_frames_from_other_sites` (:60282),
`extra_pane_intl_load_clears_pane_history_only_on_source_change` (:64518),
`starting_intl_poll_clears_previous_us_primary_display` (:61751),
`intl_poll_switch_keeps_active_same_site_history_and_clears_the_rest` (:61801).

**Recommendation:** one guard in engine `install_*` (spec §3 already says
"Cross-site guard → clear"). Normalize P3 to also emit the diagnostic —
the string exists to make the "every frame replaces the previous" failure
mode visible, and a silent poll-side clear is exactly where that failure
would hide. Keep wording greppable-identical.
**Decision:** ADOPT. One engine guard; the polled path GAINS the `history reset (site change...)` diagnostic with greppable-identical wording.

---

## D4. What a cross-site clear actually clears (blast radius)

| Path | Cleared | Evidence |
|---|---|---|
| P1 | `clear_frame_history` (:8173): history, cursor→0, `low_sweep_disabled_cuts`, `manual_primary_cut_hold`, playing/browsing→false, `last_history_step`, **loop render cache** (:8194), **storm tracker + tracks + cells caches + rotation markers** (:8183-8191), remembers per-product cut first (:8174) | :8271 |
| P2 | `ViewPane::clear_history` (:4374): history, cursor→0, playing/browsing→false, `last_history_step`, `archive_load_progress`. **No loop-render-cache, no storm state, no cut-hold equivalents** | :14439 |
| P3 | same `clear_frame_history` as P1 | :31111 |
| P4 | replace-all + `select_radar_layer_history_frame` resets `selected_cut = None` every install (:7816) | :7788, :7804 |

**Pinned by:** `storm_tracks_rebuild_from_cached_history_when_loop_extends_backward`
(:61154) exercises the storm-cache interplay;
`loop_timeline_summary_cache_invalidates_on_history_mutations` (:56539) pins
generation-driven invalidation on clear.

**Recommendation:** engine owns history/cursor/playing state; the primary-only
side effects (storm tracker, loop render cache, product-cut memory) stay in
ViewerApp keyed on an `InstallOutcome::ClearedCrossSite` — spec §3 already
requires side effects to stay out of the engine. No behavior change needed,
but the census note is: **P2 panes deliberately do NOT clear the shared
loop-render caches** — do not "fix" that during the port.
**Decision:** ADOPT. Engine owns history/cursor/playing; primary-only side effects stay in ViewerApp keyed on `InstallOutcome::ClearedCrossSite`. Panes deliberately keep NOT clearing shared loop-render caches.

---

## D5. Cursor/selection after install

| Path | Rule | Evidence |
|---|---|---|
| P1 | Select batch anchor (`selected_index`→identity, fallback = last) **only when** `select_loaded_frame && !history_playing && (!browsing_history \|\| display_is_blank)`. If not selecting: reposition cursor to the ACTIVE identity's (possibly shifted) index, status `"Backfilled {site}"` | :8281-8296, :8347-8356 |
| P2 | Same anchor select, guard `select_loaded_frame && !pane.history_playing && !pane.browsing_history` — **no blank-display escape**. Else-branch backfill reposition identical | :14451-14465, :14493-14503 |
| P3 | **Follow newest**: cursor → installed identity **unless `history_playing`** — `browsing_history` does NOT block the follow | :31133-31140 |
| P4 | Anchor select ALWAYS (`batch.selected_index` mapped through identity after sort, clamped); no playing/browsing guards (layers have no playing state; timeline_sync reselects later) | :7781-7804 |

Two P1-only sub-behaviors inside the select branch:
- `display_is_blank` also force-exits browse mode (:8325-8327) — pinned by
  `blank_browse_state_selects_loaded_latest_frame` (:61268).
- `preserve_active_frame_cut`: when the anchor IS the active frame (same
  identity AND same volume Arc), the previous cut/product are restored after
  selection (:8291-8294, :8336-8345) — P2 has NO equivalent; pane cut
  continuity is handled inside `install_extra_pane_volume_arc`'s
  selection policy instead (:14670-14682).

**Pinned by:** `live_update_does_not_steal_selection_while_history_is_playing`
(:60472), `polled_volume_same_site_upserts_and_playing_loop_keeps_its_cursor`
(:60435-60450), `blank_browse_state_selects_loaded_latest_frame` (:61268),
`same_frame_completion_preserves_manual_low_sweep_cut` (:61473),
`keyboard_frame_step_selects_history_and_updates_browse_state` (:61232).

**Recommendation:** this is THE SelectionPolicy payload. Keep three variants
per spec §3 but record the fine print in the enum docs: (a)
`FollowNewestUnlessPlaying` (P3) deliberately ignores `browsing` — browsing a
live polled feed and getting yanked to newest on the next tick is current,
owner-visible behavior; (b) `SelectAnchor` carries the
`blank-display-overrides-browsing` escape as P1-only (`Primary` role) unless
the owner normalizes it into panes; (c) the backfill cursor-reposition (keep
the DISPLAYED frame under the cursor after indices shift) is common to P1/P2
and belongs in the engine, not the policy.
**Decision:** ADOPT. SelectionPolicy carries the three variants with the fine print pinned in enum docs; FollowNewestUnlessPlaying keeps ignoring `browsing` (owner-visible today, stays); blank-display-overrides-browsing stays Primary-only.

---

## D6. `select_loaded_frame` timing and select-time side effects

| Path | Timing | Side effects on select | Evidence |
|---|---|---|---|
| P1 | Selection happens at the END of install, after upsert+sort+trim; `record_final_decode` (true only for `Final` batches, :11250 vs :11221) flows into perf decode stats | `select_history_frame_with_options` (:8406): sets `source_path`, `history_playing &= can_step`, `install_volume_arc`, optional first-low-sweep select (:8428), **`sync_active_timeline_side_effects`** (:8431), **camera follow** (:8433), status = frame status text (+ follow label) | :8330-8335 |
| P2 | Same end-of-install timing; no `record_final_decode` parameter at all | `select_extra_pane_history_frame_with_options` (:14578): cursor, `history_playing &= can_step`, `install_extra_pane_volume_arc`, first-low-sweep select, pane status = frame status text. **No timeline sync, no camera follow** | :14491 |
| P3 | No separate select step — display install is unconditional (D9) | `install_volume_arc` directly (:31141) with `record_final_decode=true` but `timings=None` (so perf record is a no-op) | :31141 |
| P4 | Select happens inside the same call, immediately after replace+sort | `select_radar_layer_history_frame` (:7807): cursor, `selected_cut = None`, `source_path`/`load_timing`/`volume` from frame, texture retention per `timeline_sync` (D9) | :7804 |

- P1 History-update batches pass `record_final_decode=false, select_frame`
  from the wire (:11221); Final batches pass `(true, true)` (:11250). P2's
  History passes `select_frame` through (:14321); Final passes `true`
  (:14352).
- `select_first_low_sweep` is suppressed in P1 when the anchor is the active
  frame with the same Arc (:8328-8329); P2 always passes `true` via the
  non-options wrapper (:14575).

**Pinned by:** `blank_browse_state_selects_loaded_latest_frame` (:61268,
returns-true contract), `independent_pane_frame_step_preserves_zoom_for_same_site_loop`
(:60590), `install_writes_derived_advanced_volume_back_into_frame_history`
(:66064), `pane_install_writes_derived_advanced_volume_back_into_pane_history`
(:66108).

**Recommendation:** engine `install_batch` returns `InstallOutcome`
(selected / backfilled / deferred / cleared) and ViewerApp keeps the
role-specific select side effects (timeline sync, camera follow) — exactly
the spec §3 "StepOutcome drives side effects" pattern applied to install.
The `record_final_decode` flag is perf-telemetry plumbing, not policy: pass
it through unchanged.
**Decision:** ADOPT. `install_batch` returns InstallOutcome; select-time side effects (timeline sync, camera follow) stay role-specific in ViewerApp; record_final_decode passes through unchanged.

---

## D7. Live-partial deferral (replace-same-path / wait-for-product rules)

| Path | Deferral | require_selected_cut source | On defer | Evidence |
|---|---|---|---|---|
| P1 | `should_defer_live_partial_selection_for_active_product` guards the select | `manual_primary_cut_hold` matching the CANDIDATE identity | cursor → active identity's index; status `"Waiting for {product} in {site}"`; **return false** (no select) | :8297-8324 |
| P2 | Same helper | `pane.cut.is_some()` (any pinned tilt), current cut = `pane.cut.unwrap_or(self.selected_cut)` — **falls back to the PRIMARY's cut** | same reposition + same wording into `pane.status`; return false | :14466-14490 |
| P3 | **None** — polled frames are always `FrameStatus::Complete` (:31118), so deferral is structurally unreachable | — | — | :31113-31141 |
| P4 | **None** — no deferral logic; overlay live-partial handling is upstream (worker called with `display_live_chunk_updates=false`, :7576) | — | — | — |

- The helper (:50003) defers only when the CANDIDATE is LivePartial, same
  site, the active volume can materialize the product, and the candidate
  cannot (cut-precise when `require_selected_cut`).
- Both P1 and P2 leave the batch INSTALLED in history when deferring — only
  selection is withheld; the next completion re-enters install and selects.

**Pinned by:** `live_partial_defer_waits_for_the_selected_cut_not_any_product_cut`
(:55693), `live_partial_defer_accepts_uninserted_advanced_product_sources`
(:55737), `pinned_independent_velocity_pane_defers_live_partial_without_selected_cut`
(:55894, drives P2 at :55937),
`live_partial_without_selected_velocity_does_not_switch_visible_frame_to_reflectivity`
(:61312), `live_partial_with_half_built_low_velocity_keeps_previous_visible_frame`
(:61367), `live_partial_with_complete_low_velocity_switches_visible_frame`
(:61418), `live_partial_selection_skips_sector_chunk_cut_when_chunks_are_off`
(:60216), `synced_velocity_pane_does_not_flip_to_ref_on_ref_only_live_partial`
(:55830).

**Recommendation:** keep as engine behavior behind `SelectionPolicy` — the
P1-vs-P2 difference in `require_selected_cut` provenance (manual hold vs
pinned tilt vs primary-cut fallback) is a genuine role difference and must
be a policy parameter, not normalized. The US live-partial chain is
byte-identical through Phase 4 (spec §9), so any restructuring here waits
for 4e's differential gate anyway.
**Decision:** ADOPT. require_selected_cut provenance is a SelectionPolicy parameter (genuine role difference); the US live-partial chain stays byte-identical until 4e's differential gate.

---

## D8. History limit growth (grow-limit-for-batch) and trim

| Path | Growth | Trim | Evidence |
|---|---|---|---|
| P1 | AT INSTALL, two triggers: (a) ORD archive loop — batch >1 and every path starts `"ord-archive:"` (:51479) → `limit = max(limit, batch.len()).min(2000)`; (b) local autoplay — batch >1, all `FrameStatus::Local`, one-shot `pending_local_autoplay` flag (set :15265) | `trim_frame_history` (:8387): **writes the normalized limit back** to `self.history_frame_limit`, drops from the FRONT (oldest after sort), clamps cursor | :8225-8247 |
| P2 | **None at install.** | `trim_extra_pane_history` (:14544): reads the PRIMARY's `self.history_frame_limit`, does **not** write back; same front-drop + cursor clamp | :14449 |
| P3 | **None at install** (Load Loop requests `history_frame_limit.max(2)` frames up front, :31296) | same `trim_frame_history` as P1 | :31132 |
| P4 | n/a — no limit, no trim; window loads cap upstream via `max_frames` | none | :7788 |
| (adjacent) | SMHI/generic intl window loads grow the limit at LOAD START instead: `self.history_frame_limit = max(limit, max_frames)` in `archive_browser.rs:1041`; ORD Unified-Player window loads do BOTH (:31702 pre-grow AND the ORD install-time grow) | | |

So the same user need — "the batch must fit" — is implemented **three
different ways**: install-time path-sniffing (ORD/local), load-start
pre-grow (SMHI/ORD window), and request-sized-to-limit (intl Load Loop).

**Pinned by:** `ord_archive_batch_grows_history_limit_to_fetch_count`
(:62921 — limit 7 → 10, all 10 land, anchor 9 selected),
`same_site_pane_follows_primary_volume_and_trims_history` (:64823),
`player_frame_limit_exposes_two_thousand_frame_loops` (:65016).

**Recommendation:** normalize into the engine's `HistoryLimits` (spec §3
already adds a byte budget there): `install_batch` takes a
`grow_to_fit: bool` (or the batch carries it) and the path-sniffing
`decoded_load_is_ord_archive_frame` heuristic dies — callers know when
they're delivering an archive loop; encode intent, not path prefixes.
Pane trim reading the primary's limit is a real coupling to surface in
`PaneView` construction (shared limits object), not silently copy.
**Decision:** ADOPT. HistoryLimits gains grow_to_fit intent (callers state archive-loop intent; the `decoded_load_is_ord_archive_frame` path-sniffing heuristic dies); the pane-reads-primary-limit coupling surfaces as a shared limits object in PaneView construction.

---

## D9. Display install / texture handling / render kick-off

| Path | Display install | Texture rule | Evidence |
|---|---|---|---|
| P1 | Only via selection (D5/D6) → `install_volume_arc` | keep/retarget: `should_keep_texture_for_volume_install` + retarget same-key texture to new volume ptr; ABA-poison pane keys (`volume_ptr = 0`); else `clear_texture()`; also derived-volume write-back into history (:8787-8794) | :8857-8927 |
| P2 | Only via selection → `install_extra_pane_volume_arc` | same keep/retarget/poison logic pane-locally; recenters pane map on site change (:14747-14754); **claims `pane.pin` for the volume's site unless intl** (:14739-14744) | :14701-14770 |
| P3 | **UNCONDITIONAL** — `install_volume_arc` runs even when `history_playing` (cursor stays, display shows newest until the next loop step re-installs) | same as P1 (shared fn) | :31141 |
| P4 | Selection installs `frame.volume` into `layer.volume`; **texture retained ONLY under `timeline_sync`** (held until replacement render lands), else cleared; `pending_render_key = None` always | :7811-7827 |

Render kick-off itself (the drains/workers that notice a cleared
texture/pending key and enqueue a RenderRequest) is the Phase-4a agent's
region (`poll_async_render` :11790-region, `poll_radar_layer_renders`,
prewarm family) — deliberately not deep-read here; the install-side contract
is only "clear/poison the key and the render machinery re-renders".

**Pinned by:** `same_site_refresh_keeps_existing_texture_until_replacement_render`
(:60249), `different_site_install_starts_from_default_reflectivity` (:60229),
`same_site_install_preserves_velocity_selection` (:55491),
`radar_overlay_timeline_sync_retains_visible_texture_until_replacement_render`
(:59662), `latest_load_clears_different_or_stale_display` (:63655),
`installing_volume_preserves_user_map_view` (:61594).

**Recommendation:** P3's unconditional display install is the doc-commented
intent (":31100-31102 — into the frame strip first, then onto the map") and
is what makes a polled feed feel live; but "display yanks to newest
mid-playback while the cursor holds" is a genuine UX oddity worth an owner
call. If kept (likely), the engine's `install_frame` for
`FollowNewestUnlessPlaying` must also surface "install display even when not
selecting" — an outcome flag, so the display write stays in ViewerApp.
**Decision:** OWNER DECIDED (2026-07-02): **LIVE WINS — keep today's behavior.** A polled feed's newest volume takes the display the moment it arrives, even mid-loop-playback (the cursor still holds). Now deliberate: the engine surfaces install-display-even-when-not-selecting as an outcome flag, ViewerApp does the display write, and a pinning test makes it un-regressable in either direction.

---

## D10. Dedupe keys

| Path | Install-side | Fetch-side | Evidence |
|---|---|---|---|
| P1 | identity upsert (D2) | worker gets `known_frame_paths` + `current_frame_identity`; `Unchanged{reason}` short-circuits (cache hit + same path) | :11228-11241, :65223 |
| P2 | identity upsert | same worker contract via `current_extra_pane_history_paths` (:14557); follow-primary dedupes by **Arc pointer** (`followed_primary_volume_ptr`, :7272-7275) | :14257-14267 |
| P3 | identity replace | `poll_last_file`: custom-URL = `{prefix}{entry.signature}` from dir.list (name+size, so a growing same-name file re-polls, :31786); intl = newest catalog identity (:31427-31428); `Ok(None)`→`Err("")` no-change marker (:31431); intl Load Loop hands its newest identity to the poll (:31355) | :31174-31192 |
| P4 | none (replace-all) | intl feed: `last_identity` into `fetch_intl_frame` (:7489-7493); US auto-refresh: `current_source_path` → `Unchanged` (:7565-7567) | :7526 |

**Pinned by:** `unchanged_realtime_refresh_requires_cache_hit_and_same_path`
(:65223), `dir_list_signatures_keep_growing_same_name_pollable` (:63612),
`extra_pane_live_source_dedupes_primary_site_and_rejects_foreign_ids`
(:64421), `same_site_pane_follows_primary_volume_and_trims_history` (:64823).

**Recommendation:** spec §3 already reserves `live.dedupe_key` per engine —
the three fetch-side schemes (path-set, dir.list signature, catalog
identity) are provider facts, not divergences; keep them in the feed
adapters. The Arc-ptr follow dedupe is pinned unchanged by spec §8
(`followed_primary_volume_ptr` row).
**Decision:** ADOPT. Dedupe schemes stay in the feed adapters as provider facts; engine carries one live.dedupe_key; Arc-ptr follow dedupe pinned unchanged.

---

## D11. Status/chip wording side effects

| Path | Strings written at install/drain | Evidence |
|---|---|---|
| P1 | install: `"history reset (site change to {site}, had {n} frames)"`, `"Waiting for {product} in {site}"`, `"Backfilled {site}"`; select: frame-status text (+ `" - following {label}"`); drain: `"Loaded {label}"` **only when the anchor got selected**, `"Current {label} ({reason})"`, `"Load failed for {label}: {err}"`, `"L2 load worker disconnected"`, `"Event loop rolling — {n} frames"` | :8267, :8317, :8354, :8434-8438, :11224, :11239, :11277, :11300, :11265 |
| P2 | same three install strings into `pane.status`; select overwrites with frame-status text (:14611-14614); drain: `"Playing {n} frames for {label}"` (Loop mode), `"Loaded {n} frames for {label}"`/`"Loaded {label}"` **only when NOT selected**, `"Current {label} ({reason})"`, `"Load failed…"`, `"Pane L2 load worker disconnected"` | :14437, :14483, :14500, :14361-14374, :14337, :14401 |
| P3 | drain BEFORE install: `"Polled: {name}"`; errors `"Poll: {message}"`; cross-site clear SILENT (D3); `install_volume_arc` then overwrites with frame-status text; intl loop: `"Intl loop: {name}"`, `"International loop loaded ({n} frames)"`, newest-scan-only explainer (:31368-31379) | :31180, :31186, :31111 |
| P4 | callers own it: `"Preview {label}"`, `"Loaded {label}"`, `"Current {label} ({reason})"`, `"Load failed for {label}: {err}"`, `"Layer load worker disconnected"` (US arm) vs `"Layer load worker disconnected ({label})"` (intl arm — wording drift) | :7718, :7723, :7731, :7743, :7753, :7537-7541 |

Note the P1-vs-P2 inversion: P1 writes "Loaded" when selection SUCCEEDED
(select overwrote status, then drain re-writes "Loaded {label}"); P2 writes
"Loaded…" only when selection did NOT happen (because pane select already
ended with frame-status text). Net UX is similar but the code paths cross.

**Pinned by:** `background_taiwan_cwa_refresh_does_not_hijack_the_global_status_bar`
(:52125 — the status-ownership contract the WorkerSlot rules generalize);
frame-status text format via `frame_status_text` (:51580) exercised by
`extra_pane_selected_frame_status_text` callers.

**Recommendation:** per spec §3, engines get a LOCAL `status: String` and
never the global bar; strings are chosen by the owner at drain time. Keep
every string greppable-identical during the port; fold the intl-arm
`({label})` disconnect suffix into ONE wording when the overlay drains
unify (trivial normalize, flag it in the PR).
**Decision:** ADOPT. Engines get a LOCAL status string; the global bar stays a drain-time owner concern; all strings greppable-identical through the port; the intl disconnect-suffix wording folds to one form when the overlay drains unify (flagged in the commit).

---

## D12. Generation bumps

| Path | Spine | Evidence |
|---|---|---|
| P1 | `FrameHistory` newtype (:2658): every mutation (`push`/`remove`/`clear`/`sort_by`/`iter_mut`/`IndexMut`) bumps a process-monotonic generation; no `DerefMut` by design | :1620, :2647-2706 |
| P2 | same `FrameHistory` type per pane | :4292 |
| P3 | same primary `FrameHistory` | shared fields |
| P4 | **plain `Vec<FrameHistoryEntry>` — NOT on the generation spine** | :2260 |

Spec Phase 4c explicitly makes "overlay history joins the generation spine"
a deliverable; this census confirms it is currently absent, so
overlay-history-derived caches cannot key on generation today.

**Pinned by:** `loop_timeline_summary_cache_invalidates_on_history_mutations`
(:56539), `loop_timeline_summary_cache_is_per_target` (:56643),
`frame_history_signature_changes_for_replacement_volume` (:56965),
`low_sweep_primary_summary_key_tracks_pane_histories` (:56719).

**Recommendation:** moving P4 to `FrameHistory` is the smallest-risk part of
4c (replace-all becomes `clear()`+`push()`s or `From<Vec>` — every route
bumps). Spec §9 pins the v0.28 bump discipline as build-on-never-weaken;
the debug assertion harness (spec §3) should land with this change.
**Decision:** ADOPT. Overlay history joins FrameHistory (every route bumps the generation); the debug assertion harness lands with this change; bump discipline is build-on-never-weaken.

---

## D13. Drain-level divergences that shape install behavior

| Behavior | P1 | P2 | P3 | P4 | Evidence |
|---|---|---|---|---|---|
| `History(batch, select_frame=false)` | installs, doesn't select | installs, doesn't select | n/a | **BATCH SILENTLY DROPPED** (`if select_frame { … }` with no else) | :11219-11227, :14320-14327, :7720-7725 |
| Autoplay on Final | only `event_explorer.pending_autoplay` → play + `"Event loop rolling…"` | `load_mode == Loop && frames > 1` → `start_extra_pane_history_loop_if_ready` (also on `Unchanged`) | never | never | :11260-11269, :14355-14364, :14338-14340 |
| `Unchanged` handling | keeps display, records `live_refresh_skip_reason`, clears pending state | keeps display, may start Loop playback | `Err("")` no-change marker, silently ignored | keeps display, sets status | :11228-11242, :14328-14343, :31183-31188, :7726-7733 |
| Disconnected | `"L2 load worker disconnected"` + clears pending flags | pane equivalent | clears `poll_rx` silently | layer status | :11293-11302, :14395-14403, :31190, :7751-7756 |
| Local autoplay | `pending_local_autoplay` grows the limit at install (D8); playback rolls via the same event-autoplay drain arm | — | — | — | :8236-8247 |

P4's dropped-batch arm is currently harmless-by-construction (overlay
workers are spawned with `live_preload_frame_count = 0` at :7574-7575, so
`History(_, false)` backfills never arrive) — but the code path diverges and
would bite if overlay preloads were ever enabled.

**Pinned by:** `independent_pane_loop_load_starts_playback_when_final_batch_lands`
(:60540), `live_preload_only_applies_to_explicit_latest_loads` (:63518),
`explicit_loads_pause_url_poll_auto_refresh_does_not` (:63510).

**Recommendation:** when P4 moves onto `install_batch`, give the overlay
drain the P1/P2 semantics (install regardless, select per policy) and note
it as a conscious normalize of dead behavior. Autoplay stays a drain-site
(ViewerApp) concern driven by `InstallOutcome` — never engine-internal.
**Decision:** ADOPT. Overlay drain gets the P1/P2 install-regardless semantics as a conscious normalize of dead behavior; autoplay stays a drain-site concern driven by InstallOutcome.

---

## D14. Preview-frame handling

| Path | Behavior | Evidence |
|---|---|---|
| P1 | `Preview(decoded)` → `install_preview_volume` (:8201): display ONLY, never enters `frame_history` | :11215-11218 |
| P2 | `install_extra_pane_volume`: display only, not history | :14315-14319 |
| P3 | n/a (no preview concept) | — |
| P4 | `Preview(decoded)` → **`install_radar_layer_volume` — REPLACES THE WHOLE HISTORY** with the preview frame | :7716-7719 |

**Pinned by:** `preview_policy_enables_fast_first_pixels_for_all_cpu_budgets`
(:55051) and the frame-status ladder keeping Preview lowest (:51554,
exercised by upsert tests) pin the primary side; no test pins P4's
history-replacing preview.

**Recommendation:** divergent and untested — decide explicitly. Engine-wise,
previews should be display-only in all roles (P4's replace is an artifact of
overlay having no separate display-install path). Add a pinning test either
way before 4c code lands.
**Decision:** DECIDE NOW: previews are display-only in ALL roles (the overlay replace-through-history is an artifact, not intent). Pinning test lands BEFORE the 4c port changes the path.

---

## D15. Phase-2 carryover: ORD edge-preserving window thinning vs generic intl newest-tail

The ONE known cross-provider divergence flagged by the Phase-2 report,
recorded here because both shapes feed the SAME P1 install path:

| Arm | Thinning when window > max_frames | Evidence |
|---|---|---|
| ORD Unified-Player window load | `limit_ord_archive_plans_for_window` (main.rs:3122): hour-granular retain to `[start, end]`, dedupe, then **evenly-spaced sampling that always keeps BOTH window edges** (`index = slot * last / (max_frames - 1)`) | main.rs:3122-3153; caller `fetch_ord_archive_window_frame_batch` :50763; started at `start_ord_archive_window_load` :31682 |
| Generic intl arm (SMHI + every future `archive_source()` provider) | `ArchiveFrames::window_plans` default (data_source/src/international.rs:168-208): day-granular fold (boundary-date frames outside the window ride along), dedupe, then **`split_off(len - max)` — the NEWEST tail only**; the window START edge is dropped first | international.rs:159-167 (doc states the contract), :205-207; consumed by `archive_browser.rs` `start_intl_archive_window_load` :1028 (comment :1022-1027 names the split deliberately) |

Observable difference: a 6-hour SMHI window at max=20 gives the last ~20
scans (tail-heavy loop that may not reach the window start); the same ORD
request gives 20 scans spanning the whole window with both endpoints
guaranteed. Also ORD trims to the hour; generic trims to the day.

**Pinned by:** `ord_archive_window_plan_limit_filters_and_preserves_edges`
(main.rs:55339), `default_window_plans_folds_days_oldest_first_and_caps_to_the_newest`
(data_source/src/international.rs:1208),
`ord_archive_window_load_initializes_international_primary_source` (main.rs:55359),
`event_loop_archive_object_limit_preserves_window_edges` (main.rs:55300 — the
US archive loader ALSO edge-preserves, making generic-intl the odd one out).

**Recommendation:** normalize toward edge-preserving (ORD/US shape) by
giving `ArchiveFrames::window_plans` an edge-preserving default once
`FramePlan` timestamps allow (the doc comment at international.rs:162-165
names the blocker: FramePlans carry no timestamp, so the default can only
day-trim + tail-cap). Until then this is a documented per-provider
capability difference, not silent drift — it belongs in the coordinated-loop
capability text, and the census marks it KEEP-with-comment for 4c.
**Decision:** OWNER DECIDED (2026-07-02): **EVEN SAMPLING (edge-preserving) is the one rule** for over-cap archive windows. Implement wherever frame timestamps are derivable (ORD/SMHI/NCI identities all parse — see archive_browser::intl_plan_time_utc); a provider with genuinely unparseable stamps may fall back to newest-tail as a DOCUMENTED capability limit, not silent drift.

---

## D16. Return values and repaint contracts (minor, for shim fidelity)

| Path | Returns | Repaint | Evidence |
|---|---|---|---|
| P1 | `bool` — anchor frame got selected (deferral and backfill return false) | `request_repaint` on every exit arm | :8221-8360 |
| P2 | `bool` — same semantics | same | :14415-14507 |
| P3 | `()` | via `install_volume_arc` | :31103 |
| P4 | `()` (`select_radar_layer_history_frame` returns bool, ignored here) | callers repaint after drain | :7773-7805 |

**Recommendation:** engine `InstallOutcome` supersedes the bools; the
delegating shims (Phase 4e sub-stage ii) must preserve the exact
true/false-per-arm mapping because drain status strings key on it (D11).
**Decision:** ADOPT. InstallOutcome supersedes the bools; 4e's delegating shims preserve the exact true/false-per-arm mapping because status strings key on it.

---

## Appendix A — pinned-behavior test inventory (flat)

All in `crates/app_ui/src/main.rs` unless noted; line = `fn` line at HEAD.

- `polled_volume_drops_previous_site_history` :60335 — P3 cross-site clear
- `polled_volume_history_stays_time_sorted` :60361 — P3 sort-by-identity
- `polled_volume_same_site_upserts_and_playing_loop_keeps_its_cursor` :60400 — P3 upsert-replace + FollowNewestUnlessPlaying
- `installing_new_site_batch_drops_previous_site_history` :60302 — P1 cross-site clear stops playback
- `history_scope_detects_frames_from_other_sites` :60282 — shared clear predicate
- `live_update_does_not_steal_selection_while_history_is_playing` :60472 — P1 playing guard
- `blank_browse_state_selects_loaded_latest_frame` :61268 — P1 blank-display browsing escape
- `keyboard_frame_step_selects_history_and_updates_browse_state` :61232 — browse-state transitions around installs
- `live_partial_without_selected_velocity_does_not_switch_visible_frame_to_reflectivity` :61312 — P1 deferral
- `live_partial_with_half_built_low_velocity_keeps_previous_visible_frame` :61367 — P1 deferral
- `live_partial_with_complete_low_velocity_switches_visible_frame` :61418 — P1 deferral release
- `same_frame_completion_preserves_manual_low_sweep_cut` :61473 — P1 preserve_active_frame_cut + manual hold
- `live_partial_defer_waits_for_the_selected_cut_not_any_product_cut` :55693 — defer helper, cut-precise arm
- `live_partial_defer_accepts_uninserted_advanced_product_sources` :55737 — defer helper, derived products
- `pinned_independent_velocity_pane_defers_live_partial_without_selected_cut` :55894 — P2 deferral (drives install at :55937)
- `synced_velocity_pane_does_not_flip_to_ref_on_ref_only_live_partial` :55830 — pane live-partial product hold
- `live_partial_selection_skips_sector_chunk_cut_when_chunks_are_off` :60216 — chunk-display gating
- `live_partial_history_upsert_replaces_same_path_when_radials_increase` :65238 — P1/P2 upsert tier (a)
- `frame_work_cache_key_keeps_live_partial_replacements_distinct` :60980 — replacement volumes get distinct cache keys
- `unchanged_realtime_refresh_requires_cache_hit_and_same_path` :65223 — Unchanged short-circuit contract
- `ord_archive_batch_grows_history_limit_to_fetch_count` :62921 — P1 grow-limit-for-batch (ORD arm)
- `ord_archive_window_plan_limit_filters_and_preserves_edges` :55339 — ORD edge-preserving thinning
- `event_loop_archive_object_limit_preserves_window_edges` :55300 — US archive edge-preservation (the majority shape)
- `ord_archive_window_load_initializes_international_primary_source` :55359 — ORD window load ownership
- `ord_archive_primary_display_survives_after_async_load_installs` :62854 — display ownership across P1 installs
- `live_ord_archive_target_loop_decodes_multiple_france_frames` :62960 — (ignored, live) ORD loop batch end-to-end
- `default_window_plans_folds_days_oldest_first_and_caps_to_the_newest` — data_source/src/international.rs:1208 — generic newest-tail
- `independent_pane_loop_load_starts_playback_when_final_batch_lands` :60540 — P2 Loop-mode autoplay
- `independent_pane_frame_step_preserves_zoom_for_same_site_loop` :60590 — P2 select side effects
- `extra_pane_intl_load_clears_pane_history_only_on_source_change` :64518 — P2 load-start clear rule
- `same_site_pane_follows_primary_volume_and_trims_history` :64823 — follow-primary + pane trim reads primary limit
- `extra_pane_live_source_dedupes_primary_site_and_rejects_foreign_ids` :64421 — pane fetch-side dedupe policy
- `pane_live_poll_action_policy` :64312 — pane poll cadence/action table (adjacent contract)
- `install_writes_derived_advanced_volume_back_into_frame_history` :66064 — P1 derived-Arc write-back
- `pane_install_writes_derived_advanced_volume_back_into_pane_history` :66108 — P2 derived-Arc write-back
- `same_site_install_preserves_velocity_selection` :55491 — install_volume_arc selection continuity
- `different_site_install_starts_from_default_reflectivity` :60229 — cross-site display reset
- `same_site_refresh_keeps_existing_texture_until_replacement_render` :60249 — texture keep/retarget
- `latest_load_clears_different_or_stale_display` :63655 — stale-display clear at load start
- `installing_volume_preserves_user_map_view` :61594 — install never yanks pan/zoom (catalog sites)
- `radar_overlay_history_syncs_to_latest_frame_at_or_before_timeline` :54935 — P4 KeepCursor/timeline_sync selection
- `radar_overlay_timeline_sync_retains_visible_texture_until_replacement_render` :59662 — P4 texture retention
- `coordinated_archive_overlay_is_not_replaced_by_live_auto_refresh` :59724 — P4 archive-vs-live ownership
- `coordinated_one_frame_overlay_never_paints_a_future_final_scan` :59741 — P4 replace-all + staleness join
- `coordinated_one_frame_overlay_holds_only_within_staleness_budget` :59787 — P4 staleness budget
- `starting_intl_poll_clears_previous_us_primary_display` :61751 — P3 owner-switch clear
- `intl_poll_switch_keeps_active_same_site_history_and_clears_the_rest` :61801 — P3 same-site keep rule
- `dir_list_signatures_keep_growing_same_name_pollable` :63612 — P3 custom-URL dedupe signature
- `explicit_loads_pause_url_poll_auto_refresh_does_not` :63510 — poll-vs-explicit-load arbitration
- `live_preload_only_applies_to_explicit_latest_loads` :63518 — why P4 never sees History(_, false)
- `loop_timeline_summary_cache_invalidates_on_history_mutations` :56539 — generation spine
- `loop_timeline_summary_cache_is_per_target` :56643 — generation spine per target
- `frame_history_signature_changes_for_replacement_volume` :56965 — generation on in-place replace
- `low_sweep_primary_summary_key_tracks_pane_histories` :56719 — pane histories in summary keys
- `player_frame_limit_exposes_two_thousand_frame_loops` :65016 — MAX_HISTORY_FRAME_LIMIT surface
- `background_taiwan_cwa_refresh_does_not_hijack_the_global_status_bar` :52125 — status-ownership contract

## Appendix B — dimensions with NO divergence (verified uniform)

- Sort order: ascending `FrameIdentity` everywhere (D1).
- Identity derivation: `frame_identity_for_volume` = (site id, volume scan
  time UTC) everywhere (:51499).
- Trim direction where trims exist: drop-oldest-from-front + clamp cursor
  (:8389-8394, :14549-14554).
- Cross-site predicate where it exists: active-volume site mismatch OR
  `history_contains_other_site` — textually identical in P1/P2/P3.
