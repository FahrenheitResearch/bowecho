# BowEcho v0.29 — One Radar World: Loop Engine + Site Model Spec

**Status:** the v0.29 structural program. Written against branch `v028/unslop` @ `354a66b` (v0.28.2, `crates/app_ui/src/main.rs` = 70,600 lines, `layers_rail.rs` = 2,796 lines, 1,325 `#[test]`s workspace-wide of which 539 live in main.rs). Line numbers WILL drift under the write fleet — every reference names the symbol, which is durable. Re-verify lines by grep at implementation time.

**Inputs:** the engineering audit (`bowecho-audit-2026-07-01.md` §4 — the structural program) and the international parity audit (`bowecho-intl-parity-2026-07-02.md` — STRUCTURAL items A–E). This spec is the synthesis of a three-way judged design panel (engine-first / data-model-first / ux-first). **Winner: data-model-first sequencing** — migration safety weighs heaviest per the owner's constraints, and it was the only plan that pins current behavior with contract tests before moving anything, keeps the identity substrate stable for two phases before the engine port touches it, and adopts the engine overlay→panes→primary (smallest blast radius first). **Grafted from engine-first:** the differential test suite as the engine-port entry gate, the behavior census of the four install paths, the row-level archive union (never forcing the verified-good US S3 loader through FramePlan), the FrameHistory generation assertion harness, and the channel-ownership invariant. **Grafted from ux-first:** the render-policy posture (name both policies first, flip the overlay pool as its own revertible commit), the two-button bar reusing `UnifiedPlayerAction` verbatim as the Advanced disclosure, the reason-as-value capability enums, and the YAGNI worker-slot discipline.

---

## Read this first — what is ALREADY DONE at HEAD (do not re-plan)

Commit `c1e243c` ("International parity wave") landed most of the parity audit's quick wins. Verified present at 354a66b:

1. **`BeamTarget::Intl`** (main.rs:304-311) — intl sites rank in the lowest-beam list with real beam heights; `BeamCandidate.origin` (:322) carries provider provenance.
2. **Intl overlays are reachable again** — `add_or_refresh_intl_radar_layer` (:7398) is wired from `add_nearest_radar_overlay_at` (:7274, caller at :7293); the v0.27.2 regression is fixed.
3. **The primary mode chip is intl-aware** — `mode_chip_state` (:36734) dispatches on `intl_source_owns_primary_display()` with `INTL_STALE_CHIP_FLOOR_SECONDS = 1800` (:580) via `mode_chip_state_with_live_and_stale_floor` (:36765).
4. **R16 write-back is fixed** — advanced-product derived Arcs write back into frame history (test at ~:56880 "in-place volume write-back must invalidate the cached summary"; pane variant comment at :14566).
5. **12 of 13 intl providers loop** via the derived `recent_source()` pattern (only JMA remains single-frame, deliberately).
6. **Dead inventory is smaller than the audit's §4.3 list:** the `timeline` crate, `saved_layout_slots` (now only a removed-legacy-key test in settings/src/lib.rs:1601), and the legacy PNG sounding renderer are **already deleted**. Remaining candidates (pre-unification event-jump, dormant MTG plumbing, duplicate Vol3D controls, orphaned CWT build.rs branch, second SPC tornado-report surface) must be re-verified at Phase 5 start.

v0.29 is therefore the **structural** program, not those repairs: collapse the three loop engines, make the site model one type, unify the archive world, and put the two-button front on top.

---

## 0. The defect being fixed, stated once

The "site + frame history + texture + loop" machine exists **three times** with drift, and worker policies are opposite by accident:

| Copy | State | Install path | Stepper | Render drain | Render worker |
|---|---|---|---|---|---|
| Primary | ~30 loose ViewerApp fields (`frame_history` :1623, `poll_*` :1926-1944, `realtime_level2_auto_refresh` :2150) | `install_polled_volume` :31088 + `install_decoded_load_batch` :8189 | `advance_primary_screen_loop` :9655 | `poll_async_render` :11790 | shares the ONE coalescing worker (`spawn_render_worker` :5239) |
| Extra pane | `ViewPane` :4261-4306 (own history/cursor/texture/live flag) | `install_extra_pane_decoded_load_batch` :14284 | `advance_extra_pane_screen_loop` :9685 (near-identical body) | (same drain, keyed by `RenderRequest.pane`) | shares the same worker (doc :4255-4260) |
| Overlay | `RadarOverlayLayer` :2247-2281 — `frame_history: Vec<FrameHistoryEntry>` (:2255), **not** the v0.28 `FrameHistory` generation newtype | `install_radar_layer_volume` :7747 | timeline-sync selection (~:7785-7817) | `poll_radar_layer_renders` :11895 | **spawns its own thread+cache per layer** (`spawn_overlay_render_worker` :5247 in `::new` :2285) |

Consequences, all verified: the owner's invalidation spine (the FrameHistory generation newtype, :2653-2757) stops at the overlay boundary; the pane-0 grid chip still falls through to `realtime_level2_auto_refresh` (`pane_chip_is_live` :27748-27757 — the last live R8 residue); `IntlOverlayFeed` (:2221) is a fourth mini-engine bolted to the third; and the `thread::spawn + mpsc + Option<Receiver> + try_recv + request_repaint` idiom is hand-rolled 67× (42 `Option<mpsc::Receiver>` fields in main.rs alone).

In parallel, two site worlds: `self.sites: Vec<RadarSite>` addressed by index and gated by `site_is_primary_level2_catalog_site` (:42482, callers at :19172, :28435, :28537, :28777, :39647, :42521, event_explorer.rs:258) vs `intl_static_sites()`/`IntlProvider` (international.rs:257) reached through `PollSource::Intl` (:4064), `PaneIntlSource` (:2231), `IntlOverlayFeed` (:2221). Every remaining parity gap in the parity audit is downstream of this split.

**End-state invariants** (what "done" means):

1. One `LoopEngine`; primary, panes, and overlays are three instances differing only in declared policy.
2. One `SiteRef`; `site_is_primary_level2_catalog_site` and `site_is_tdwr` are deleted; both catalogs are private to one module; a US-only feature cannot be written without a visible match on the site kind (compile pressure, not review).
3. Every channel in the app is owned by exactly one of: `RenderService`, a `WorkerSlot`, or a `StreamSlot`. Nothing else holds a `Sender`/`Receiver`.
4. Every displayed time comes from data; every capability label comes from the provider catalog (derived, tripwire-tested); every unavailable affordance is greyed with a reason that IS the value a capability function returned.
5. The two-button LIVE|ARCHIVE bar is the primary flow; every existing power control survives behind disclosure.

---

## 1. Site model — `crates/data_source/src/sites.rs` (new)

```rust
/// One radar site anywhere on earth. Strings, never indices: survives
/// catalog reorder, serializes into settings, Eq/Hash for dedupe keys.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum SiteRef {
    /// Embedded US Level-II catalog id (WSR-88D, TDWR, research feeds).
    Us { level2_id: String },                       // "KTLX", "TOKC", "KCRI"
    /// data_source::international registry site.
    Intl { provider_id: String, site_id: String },  // {"ord","deess"}, {"smhi","angelholm"}
}

/// The exhaustive-match lever. Deliberately NOT #[non_exhaustive]:
/// adding a variant is a compile error at every consumer — that is the
/// type-system parity outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SiteKind {
    Wsr88d,                       // incl. TJUA — the exception becomes catalog data
    Tdwr,                         // catalog data, NOT a 'T'-prefix heuristic
    Research,                     // community_feed_for_site() != None today
    Intl { provider_id: String },
}

/// Resolved site row — what pickers, beam rankings, markers, engines consume.
#[derive(Clone, Debug, PartialEq)]
pub struct SiteRecord {
    pub site: SiteRef,
    pub kind: SiteKind,
    pub label: String,            // "KTLX Oklahoma City" | "Ängelholm"
    pub origin: Option<String>,   // provenance suffix ("SMHI Sweden") — BeamCandidate.origin (:322) anticipated this
    pub lat_lon: Option<(f32, f32)>,
    pub country: String,
}

impl SiteRef {
    /// Settings/favorites encoding. Backward compatible: US ids never
    /// contain ':', so every existing settings value parses as Us.
    /// Case is PRESERVED for intl keys (ORD/SMHI ids are lowercase).
    pub fn settings_key(&self) -> String;            // "KTLX" | "intl:smhi:angelholm"
    pub fn parse_settings_key(s: &str) -> SiteRef;   // ':'-free => Us
}

/// THE catalog API. After Phase 3, feature code never iterates
/// self.sites or intl_static_sites() directly again — both become
/// private to this module (enforcement by privacy, not grep).
pub fn resolve(site: &SiteRef) -> Option<SiteRecord>;   // no network (US embed + intl_static_sites :257)
pub fn all_sites() -> impl Iterator<Item = SiteRecord>; // union, registry order
pub fn sites_near(lat: f32, lon: f32, radius_km: f32) -> Vec<(SiteRecord, f32)>;
```

### 1.1 The settings-uppercasing trap (MUST fix before any intl key is written)

`AppSettings::add_favorite` (settings/src/lib.rs:765-768) calls `to_ascii_uppercase()` and `is_favorite`/`remove_favorite` compare `eq_ignore_ascii_case`. An `"intl:ord:deess"` key would be corrupted to `"INTL:ORD:DEESS"` on write — ORD/SMHI site ids are case-significant lowercase. Fix in Phase 1: keys containing `':'` are stored verbatim and compared case-sensitively; bare keys keep today's uppercase behavior byte-for-byte. Round-trip tests including case are a Phase 1 gate item. `AppSettings` stays Eq-derivable by storing **encoded strings, never the enum** — no serde of `SiteRef` into settings, ever.

### 1.2 Archive capability — generalize the proven `recent_source()` pattern

Mirror of `RecentFrames`/`recent_source()` (international.rs:140-208, the documented single override point, derived `supports_recent` :206, tripwire test at :1012):

```rust
// crates/data_source/src/international.rs — additions beside recent_source()
pub trait ArchiveFrames {
    /// All frames for `site_id` on `date`, oldest first. Catalog probes
    /// only — never volume downloads (same cheapness contract as recent).
    fn day_plans(&self, site_id: &str, date_utc: NaiveDate) -> Result<Vec<FramePlan>, String>;
    /// Frames inside [start,end], oldest first, capped. Default folds
    /// day_plans over covered dates; hour-granular listers (ORD) override.
    fn window_plans(&self, site_id: &str, start: DateTime<Utc>, end: DateTime<Utc>,
                    max: usize) -> Result<Vec<FramePlan>, String> { /* default */ }
}

pub trait IntlProvider {
    // ...existing, unchanged...
    /// THE single override point for archive lookup.
    fn archive_source(&self) -> Option<&dyn ArchiveFrames> { None }
    fn supports_archive(&self) -> bool { self.archive_source().is_some() }
}
```

Initial impls wrap existing code verbatim: `OrdProvider` over `archive_plans_for_hour` (international/ord.rs:313); `SmhiProvider` over `smhi_archive_plans_for_day` (international/smhi.rs:72). The hand-maintained `archive_lookup: bool` capability field (international.rs:282, drift documented in the parity audit — SMHI's card says false above its own working loader) becomes **derived** from `supports_archive()`, with a tripwire test pinning the id set `{"ord", "smhi"}`. FMI bucket walks, NCI tarlists, DMI STAC, JMA dated tars are later pure adapter work that flips cards automatically.

### 1.3 Capability answers are values, not missing branches

```rust
// app_ui side — the one place gates ask their questions
pub(crate) enum LoopAccess    { Recent { max_useful: usize }, SingleFrame }
pub(crate) enum ArchiveAccess {
    Level2S3,                          // full US archive (level2_objects_for_date, lib.rs:511)
    Provider,                          // provider.archive_source().is_some()
    None { reason: &'static str },     // the greyed-UI hover text IS this value
}
pub(crate) fn loop_access(site: &SiteRef) -> LoopAccess;
pub(crate) fn archive_access(site: &SiteRef) -> ArchiveAccess;
```

The archive browser, the Event Loop Builder's Build button, and mosaic candidate rows grey with a reason derived from the same call that would have powered them — the gate and its explanation cannot drift.

### 1.4 What migrates onto `SiteRef` (Phase 3 consumer list)

- `BeamTarget`/`BeamCandidate` (:304-325) — ranking iterates `sites_near()`; the `Conus(usize)` index dies.
- Favorites (star + chip row, `is_favorite` gates) via `settings_key()`; bare legacy ids parse as `Us` forever.
- Pane identity: `ViewPane.pinned_site_id` + `intl_source` (:4268-4272) collapse to `pin: Option<SiteRef>` (None = follow primary). This alone fixes the mislabeled-US-site pane combo (`extra_pane_selected_site_index` `unwrap_or` fallback) and the Follow-primary `None==None` no-op.
- `EventLoopRadarPlan.site_id: String` (event_loop_builder.rs:73) → `site: SiteRef`; the `to_ascii_uppercase` on input goes kind-aware (lowercase ORD codes become typable).
- Event-jump candidates (event_explorer.rs:258) filter `kind == Wsr88d` **explicitly** — stays honest-NA for US-only event data, flips automatically if an ESWD source ever lands.
- Mosaic candidate pool (`nearby_coordinated_overlay_sites` :19160) unions both catalogs tagged by kind.
- `site_is_tdwr`'s `starts_with('T')` heuristic (:42491) becomes catalog data — ends the JMA TAKA/TANE/TOJI leak permanently.
- Startup restore: persist the display owner as one encoded string; absent = legacy behavior.

---

## 2. Feed model — live vs archive is a property of the SOURCE

Replaces the scattered `poll_source` + `poll_active` + `realtime_level2_auto_refresh` flag choreography whose disagreement is the R8 bug class:

```rust
// crates/app_ui/src/loop_engine.rs
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FeedSource {
    /// Follow the newest data for a site on the source's cadence
    /// (US chunk chain or IntlProvider::latest).
    Live(SiteRef),
    /// A fixed UTC window. Never refreshes, never polls — an archive
    /// engine structurally CANNOT claim LIVE. GO LIVE is an explicit
    /// set_feed(Live(same site)).
    Archive { site: SiteRef, window: ArchiveWindow },
    /// GR2A-style dir.list poll. Primary only. Kept verbatim.
    CustomUrl(String),
    /// Drag-drop / file dialog / data packs.
    LocalFiles { label: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArchiveWindow {
    pub start_utc: DateTime<Utc>,
    pub end_utc: DateTime<Utc>,
    pub anchor_utc: Option<DateTime<Utc>>,  // loop-ending-at-scan anchor
    pub max_frames: usize,
}
```

`PollSource` (:4064) is re-expressed as `FeedSource` with `save_to_settings`/`intl_from_settings` preserved as shims — the `poll_url`/`intl_provider`/`intl_site` settings fields are unchanged (Eq-derivable, no f32, per-variant writes that never forget the other world).

**`switch_policy` — one home for every history-clear rule** (pure function, unit-testable):

```rust
fn switch_policy(old: &FeedSource, new: &FeedSource) -> SwitchAction; // KeepHistory | ClearAll
```

The existing regression tests move onto it: `start_intl_poll`'s same-site-keep/cross-site-clear (:31560 region, test `starting_intl_poll_clears_previous_us_primary_display`), `install_polled_volume`'s cross-site clear (:31088 region via `history_contains_other_site` :52024), and the pane clears (`start_extra_pane_intl_load` :13941 region). This also structurally fixes display-ownership incoherence (parity QW13): a US archive load SETS `FeedSource::Archive`, so there is no stale `poll_source` to forget.

---

## 3. LoopEngine — `crates/app_ui/src/loop_engine.rs`

`FrameHistory` (:2653-2757), `FrameHistoryEntry` (:2628), `FrameIdentity` (:2821), and `next_frame_history_generation` (:2642) move here **verbatim**. The generation newtype is the invalidation spine; the bump discipline (every mutating accessor bumps, `iter_mut` bumps up front, no `DerefMut`) survives unchanged. Overlays upgrade from `Vec<FrameHistoryEntry>` (:2255) to the newtype — the spine finally reaches the overlay boundary.

```rust
/// One "site + frame history + textures + poll cadence + playback" engine.
/// Instantiated once for the primary view, once per extra pane, once per
/// radar overlay layer. Differences are DECLARED POLICY, not divergent code.
pub(crate) struct LoopEngine {
    id: EngineId,                    // stable u64; also the render lane payload
    role: EngineRole,                // Primary | Pane { slot: u8 } | Overlay
    feed: FeedSource,
    history: FrameHistory,           // the v0.28 generation newtype, verbatim
    cursor: FrameCursor,             // index, playing, browsing, last_step
    limits: HistoryLimits,           // frame cap (MAX_HISTORY_FRAME_LIMIT=2000, :182)
                                     // + byte_budget: Option<usize>  ← NEW, lands WITH the
                                     // engine (audit §5: frame histories are the dominant
                                     // unbudgeted store; unification makes growth symmetric)
    live: LiveState,                 // enabled flag, last_refresh: Option<Instant>,
                                     // dedupe_key (poll_last_file / IntlOverlayFeed.last_identity)
    poll: WorkerSlot<PolledVolumeResult>,     // one-shot live tick (poll_rx :1929, feed rx :2227)
    loads: StreamSlot<AsyncLoadResult>,       // archive/loop batches (load_receiver :1728/:2272/:4293)
    loop_stream: StreamSlot<IntlLoopFrameMessage>,  // intl recent()/window streams (:1934)
    tex: TextureSlot,                // texture, texture_key, pending_render_key, render/worker/texture_ms
    status: String,                  // engine-local status; NEVER the global bar
}

pub(crate) enum EngineRole { Primary, Pane { slot: u8 }, Overlay }
struct FrameCursor { index: usize, playing: bool, browsing: bool, last_step: Option<Instant> }

impl LoopEngine {
    // ── feed switching: the ONE entry point for changing what is displayed ──
    fn set_feed(&mut self, feed: FeedSource) -> SwitchAction;   // applies switch_policy

    // ── install: the ONE upsert path (today ×4 — see behavior census §7.2) ──
    /// Cross-site guard → clear; upsert by FrameIdentity; sort by identity;
    /// trim to limits (trim_frame_history :8361 + NEW byte budget);
    /// cursor moves per policy.
    fn install_frame(&mut self, frame: FrameHistoryEntry, select: SelectionPolicy) -> InstallOutcome;
    fn install_batch(&mut self, frames: Vec<FrameHistoryEntry>, select: SelectionPolicy) -> InstallOutcome;

    // ── playback: the ONE stepper (today ×2 + overlay time-sync) ──
    /// Pure over (history, cursor, sweep ctx). Both steppers already share
    /// next_frame_index_with_sweep_cuts + sweep_cuts_for_history_entry; the
    /// engine wraps exactly that. Side effects (pane sync, satellite/model
    /// sync, camera follow) stay in ViewerApp, driven by StepOutcome — the
    /// engine never reaches into siblings.
    fn advance_loop(&mut self, sweep: &SweepContext) -> StepOutcome;
    /// Timeline sync for overlays (replaces ~:7785-7817) and panes:
    /// nearest frame at-or-before t.
    fn select_frame_nearest(&mut self, t: DateTime<Utc>) -> Option<usize>;
    fn select_frame(&mut self, index: usize, opts: SelectOptions);

    // ── liveness: ONE derivation kills the R8 class ──
    /// Derived ONLY from engine-own state: feed variant + live.enabled +
    /// newest frame age. Replaces pane_chip_is_live (:27748 — pane 0 still
    /// falls through to realtime_level2_auto_refresh, the last R8 residue)
    /// and mode_chip_state (:36734). Stale floor per feed kind:
    /// max(user stale_chip_seconds, INTL_STALE_CHIP_FLOOR_SECONDS,
    ///     2 × poll_cadence) for intl; US behavior unchanged.
    fn liveness(&self, now: DateTime<Utc>) -> Option<Liveness>;  // Live{age,stale} | Archive{age}

    // ── poll cadence + tick: generalization of the pure pane_live_poll_action (:4404) ──
    fn poll_cadence(&self) -> Duration;
    //   (Primary, CustomUrl | Us Live)  → 1 s chunk cadence     — byte-identical through Phase 4
    //   (Pane|Overlay, Us Live)         → 5 s overlay cadence
    //   (any, Intl Live)                → 60 s / provider cadence
    /// SharedWithPrimary becomes SiteRef equality vs primary.feed — uniform
    /// for US AND intl, which is exactly parity QW9 (intl double-poll
    /// dedupe) falling out of the design.
    fn live_tick(&self, primary: Option<&LoopEngine>, now: Instant) -> LiveAction; // Skip | Poll | FollowPrimary

    // ── rendering: engines hold lanes, never channels ──
    fn lane(&self) -> LaneId;
    fn needs_render(&self, key: &TextureKey) -> bool;
    fn begin_render(&mut self, key: TextureKey);
    /// ONE accept state machine replacing the drain bodies at :11790 and
    /// :11895 (is_latest → install; stale-ok → recycle; err-latest → clear
    /// pending + engine status). NOTE: the prewarm drain (:11850) is NOT
    /// routed through this — it inserts into loop_render_cache, a different
    /// contract; it moves verbatim into render_service.rs (§4.2).
    fn accept_render(&mut self, msg: AsyncRenderResult) -> RenderAccept;
}
```

**`SelectionPolicy` captures the real divergences** (verified against the four install bodies): `FollowNewestUnlessPlaying` (primary poll), `SelectAnchor { identity }` (batch loads select once at end, only when `!playing && !browsing`), `KeepCursor` (overlay under `timeline_sync` holds the archive cursor). The Phase 4 behavior census (§7.2) is the authority on this list — if a divergence doesn't fit these three, the census decides keep-as-policy vs normalize, per row, before any code merges.

**How the three views become instances:**

```rust
struct ViewerApp {
    primary: LoopEngine,                 // wraps the fields at :1623-1716/:1926-1944 — moved, not rewritten
    panes: Vec<PaneView>,                // was extra_panes: Vec<ViewPane>
    overlays: Vec<OverlayView>,          // was radar_layers: Vec<RadarOverlayLayer>
    render: RenderService,
    // ...
}
/// Display config stays OUTSIDE the engine — the engine is the loop
/// machine, the view is what you point at it.
struct PaneView    { engine: LoopEngine, pin: Option<SiteRef>, product: DisplayProduct,
                     cut: Option<usize>, map_center_lat: f32, map_center_lon: f32,
                     map_scale: f32, followed_primary_volume_ptr: Option<usize> /* :4283, unchanged */ }
struct OverlayView { engine: LoopEngine, opacity: u8, visible: bool,
                     timeline_sync: bool, selected_cut: Option<usize>, radar_range_km: f32 }
// IntlOverlayFeed (:2221-2228) dissolves: provider/site → engine.feed,
// last_identity → engine.live.dedupe_key, rx → engine.poll.
```

**Generation assertion harness** (debug builds): any engine method that touches entries asserts the generation changed; any read-only method asserts it didn't. Catches both a skipped bump (stale caches) and bump-on-read (thrash).

---

## 4. Worker model

### 4.1 `WorkerSlot` / `StreamSlot` — `crates/app_ui/src/worker_slot.rs` (new, ~250 lines)

The one grammar for the 67× `thread::spawn` / 42× `Option<mpsc::Receiver>` idiom:

```rust
/// At most one in-flight job; cancel = drop the receiver (the job's next
/// send errors and it exits) — IDENTICAL to today's `self.x_rx = None`
/// contract (e.g. cancel_extra_pane_load_for_user_command), so
/// cancellation semantics are preserved exactly.
pub(crate) struct WorkerSlot<T> {
    rx: Option<mpsc::Receiver<T>>,
    started: Option<Instant>,
    label: &'static str,       // background-activity panel enumerates slots
}
pub(crate) struct WorkerTx<T> { /* sender + egui::Context */ }
impl<T> WorkerTx<T> { pub fn send(&self, v: T) -> Result<(), Cancelled>; } // send + request_repaint

impl<T: Send + 'static> WorkerSlot<T> {
    pub fn idle(label: &'static str) -> Self;
    pub fn spawn(&mut self, ctx: &egui::Context,
                 job: impl FnOnce(WorkerTx<T>) + Send + 'static) -> bool; // false if in flight
    pub fn poll(&mut self) -> SlotPoll<T>;   // Idle | Pending | Ready(T) | Disconnected — never blocks
    pub fn in_flight(&self) -> bool;
    pub fn cancel(&mut self);
}

/// Streaming variant: stays busy until a terminal message
/// (T: SlotMessage { fn is_terminal(&self) -> bool }). Fits
/// AsyncLoadResult batches and IntlLoopFrameMessage streams.
pub(crate) struct StreamSlot<T: SlotMessage> {
    /* same shape */ pub fn drain(&mut self, budget: Duration) -> (Vec<T>, StreamState);
}
```

Two rules the type enforces: **(1)** a job never writes app state or the global status bar — it only sends `T`; status strings are chosen by the owner at drain time (deletes the Taiwan-writes-global-status vs Italy-doesn't drift class permanently); **(2)** every `Ready`/`Disconnected` transition clears `rx` — no half-drained states.

**YAGNI scope:** Phase 1 migrates only the verified low-risk one-shot slots (`update_check_rx` :2171, `self_update_rx` :2177, `intl_sites_rx` :1951, `coverage_probe_rx` :1959, `ord_archive_list_rx` :1968, `italy_dpc_latest_rx` :1637, `taiwan_cwa_latest_rx` :1642, `radar_operational_status_rx` :1813, `spc_receiver` :1816, `upper_air_rx` :1887). The remaining ~30 migrate opportunistically as their modules are touched — never a big-bang slot sweep. The engine-adjacent channels (`poll_rx`, `intl_loop_rx`, `load_receiver` family) migrate only inside Phase 4, with the engine.

### 4.2 RenderService — `crates/app_ui/src/render_service.rs`

**Policy resolution** (the audit's "opposite worker policies"): the pane policy (shared, coalescing, bounded) is correct and wins; overlays keep their cache-mode isolation but stop owning threads. Adopted **in two separately-revertible steps** (ux-first's posture):

- **Step 1 — name the policies, zero behavior change.** `RenderRequest.pane: usize` (:4438) becomes `lane: LaneId`; `merge_render_request` (:12735) already coalesces per pane id and generalizes per-lane unchanged; a `render_route_for(role)` function is the ONE documented policy site. Overlays still own their workers after this step.
- **Step 2 — the overlay pool flip, its own commit, telemetry-gated.** Per-layer threads (`spawn_overlay_render_worker` :5247) are replaced by one overlay pool; revert = revert one commit.

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum LaneId { Primary, Pane(u8), Overlay(u64), Prewarm }

pub(crate) struct RenderService {
    interactive: RenderPool,  // EXACTLY today's one coalescing worker, Primary cache mode
                              // (spawn_render_worker_with_mode :5336) — lanes Primary + Pane(n).
                              // Stays a separate single worker so overlay bursts can never
                              // starve the primary (today's implicit guarantee, made explicit).
    overlay: RenderPool,      // K workers, Overlay cache mode, shared queue, per-lane
                              // coalescing. K = min(active overlays,
                              // loop_prewarm_worker_count(threads) :5314). Diagnostics-visible
                              // constant = the escape hatch for low-core field regressions.
    prewarm: PrewarmPool,     // existing pool (:5255) UNCHANGED, lane Prewarm; its drain
                              // (poll_loop_prewarm_renders :11850) moves VERBATIM — it inserts
                              // into loop_render_cache and is NOT forced through accept_render.
    recycle: RecycleRouter,   // per-pool recycle channels, as today
}
impl RenderService {
    fn submit(&self, lane: LaneId, req: RenderRequest);                        // routes by lane
    fn drain(&mut self, budget: Duration) -> Vec<(LaneId, AsyncRenderResult)>; // replaces :11790 + :11895
}
// ViewerApp::update: for (lane, msg) in render.drain(12ms)
//   { engine_for_lane(lane)?.accept_render(msg) }
// A retired lane (closed pane, removed overlay) misses the lookup and the
// buffer recycles — deleting the per-layer "worker disconnected" states.
```

**Ownership invariant:** `RenderService` owns all render threads and channels; `LoopEngine`s own `LaneId`s and `TextureSlot`s; ViewerApp owns both and routes. `WorkerSlot`/`StreamSlot` own every non-render channel and live inside the engine or feature that spawned them.

---

## 5. One archive world — row-level union, not byte-level

US archive listings are `S3Object`s (lib.rs:80) with a verified-good chunked download path (`level2_objects_for_date`/`_for_window`, lib.rs:511/:546 — 00Z boundaries, anchor selection, all tested). **Do not rewrite them into FramePlans.** Unify at the row level:

```rust
// crates/app_ui/src/archive_browser.rs
enum ArchiveLister {
    Us   { level2_id: String },                       // wraps level2_objects_* verbatim
    Intl { provider_id: String, site_id: String },    // via IntlProvider::archive_source()
}
struct ArchiveScanRow { time_utc: DateTime<Utc>, load: ArchiveScanLoad }
enum ArchiveScanLoad { UsObject(S3Object), IntlPlan(FramePlan) }
impl ArchiveLister {
    fn list_day(&self, date: NaiveDate) -> /* WorkerSlot job → */ Vec<ArchiveScanRow>;
    fn list_window(&self, w: &ArchiveWindow) -> Vec<ArchiveScanRow>;
}
```

**ONE archive-browser widget** — date nav + hour-grouped minute chips + loop-ending-at-scan + "+N older" — cloned from `ord_archive_section` (layers_rail.rs:2319-2528, which already mirrors the US browser), consuming `ArchiveScanRow` and dispatching loads on the arm. It replaces: the DATA-tab `archive_panel` listing (hard-wired to `self.selected_site()` today — the silently-targets-stale-US-site dishonesty), `ord_archive_section`, and the SMHI coverage Load-hour/day buttons (layers_rail.rs:2042-2083). Loaders: US rows go down the existing shared Level-II archive loader; intl rows go through `fetch_intl_frame_plan_batch` (:51238), generalized as the one plan-batch fetcher. The four duplicated US archive-worker bodies (audit §4.3) collapse onto `StreamSlot` + `ArchiveLister` here.

---

## 6. Extraction plan — order, seams, sizes

Playbook = layers_rail: sibling module in the same binary crate, `impl ViewerApp` blocks + `use crate::{...}`, **verbatim moves, extraction commits never mixed with behavior commits, each landed within days** (c1e243c alone shifted parity-region line numbers by 300-400 — rebase-heavy work means the slice is too big).

| # | Module | Phase | What moves / is new | ~Lines out of main.rs |
|---|---|---|---|---|
| 1 | `worker_slot.rs` | 1 | NEW slot types + ~10 low-risk slot migrations in place | ~300 net |
| 2 | `data_source/src/sites.rs` | 1 | NEW SiteRef/SiteKind/SiteRecord/resolve/all_sites/sites_near; `community_feed_for_site` (:42467) moves DOWN into data_source (its table lives there already) | ~600 new |
| 3 | `international.rs` additions | 1 | ArchiveFrames + archive_source() + derived supports_archive + ORD/SMHI impls + tripwire | ~250 new |
| 4 | `archive_browser.rs` | 2 | NEW widget + ArchiveLister/ArchiveScanRow; absorbs archive_panel listing (~:14993-15188 region), ord_archive_section, SMHI coverage loaders; plan-batch fetcher family (`fetch_intl_frame_plan_batch` :51238 + ORD/SMHI one-offs + 4 US archive-worker bodies) | ~1,800 |
| 5 | `sites_ui.rs` | 3 | BeamTarget/BeamCandidate/best_radar_candidates (:28426)/intl_radar_candidates (:28569)/nearest_site_to_position; favorites encode/decode; site search; the six gate callers rewritten as SiteKind matches; `site_is_primary_level2_catalog_site` + `site_is_tdwr` DELETED | ~1,800 |
| 6 | `render_service.rs` | 4a/4b | spawn_render_worker family (:5239-5450), merge_render_request (:12735), RenderRequest/RenderedTexture/recycle types (:4433+), the three drains (:11790/:11850/:11895 — prewarm drain verbatim), LaneId | ~1,400 |
| 7 | `loop_engine.rs` | 4c | FrameHistory/FrameIdentity/generation (verbatim), DecodedLoad/Batch (:2604-2625), trim/upsert/history_contains_other_site (:52024), FeedSource + switch_policy, LoopEngine struct + differential suite | ~1,300 |
| 8 | `overlays.rs` | 4c | RadarOverlayLayer→OverlayView, add_or_refresh_* (:7356/:7398), start/poll_intl_radar_layer_loads, install_radar_layer_volume (:7747), timeline-sync selection | ~2,000 |
| 9 | `panes.rs` | 4d | ViewPane→PaneView, maybe_refresh_extra_panes (:7163), extra_pane_live_source (:7128), follow_primary_volume_into_pane (:7231), start_extra_pane_intl_load (:13941), install_extra_pane_decoded_load_batch (:14284), pane context swap (:13832, kept — see risks), pane UI + tests (:63873+) | ~3,500 |
| 10 | `primary_feed.rs` | 4e | poll_feed/drain_polled_volume/poll_source_armed/ownership predicates (:31132-31206), install_polled_volume (:31088), start_intl_poll (:31560), intl loop streaming | ~2,200 |
| 11 | `live_archive_bar.rs` | 5 | NEW two-button bar | ~400 new |

Net trajectory: 70,600 → ~62k after Phases 1-3 → low-50k after Phases 4-5 + deletions. Re-attempt **fat LTO in CI** at the end of Phase 4 (audit §4.4: fat/1 builds locally in 7m43s, ships 17% / 10 MB smaller; the CI linker OOM is a monolith cost).

---

## 7. Phase plan

**Every phase gate, uniformly:** `cargo test --workspace` green (1,325 at HEAD; never below 1,293; growing each phase) · `cargo fmt --check` + `clippy -D warnings` clean · a v0.28 settings file loads unchanged (round-trip test) · the owner builds a **release-fast exe locally and runs the named checkpoint** (no GitHub releases until v0.29.0; each phase tags `v0.29.0-alpha.N` locally). Extraction commits and behavior commits never mix. No phase depends on a later one to be shippable.

### Phase 1 — Pin behavior + slots + foundations *(work order in §11)*

Scope: (a) **contract harness** — characterization tests pinning current behavior BEFORE anything moves: chip truth table (primary US/intl × live/archive/stale-floor via `mode_chip_state*`; `pane_chip_is_live` incl. the pane-0 fall-through as-is), history-clear rules for every switch pair, pane dedupe/cadence (extend the :63873/:63935 family), settings round-trips (favorites case, `intl_provider`/`intl_site`, `poll_url`, `startup_site`). (b) `worker_slot.rs` + the ten named low-risk slots. (c) `sites.rs` + `ArchiveFrames`/ORD/SMHI impls + derived capability card + tripwire `{"ord","smhi"}` + the settings-uppercasing fix. Zero UI change except the SMHI/NCI capability cards flipping honest.

Gate: all of the above tests green; derived-badge tripwire green; grep-guard "no new direct `level2_objects_for_date` callers in app_ui outside the archive module list".
Owner checkpoint: normal live storm session + Data-tab coverage explorer — nothing should feel different except honest capability cards.

### Phase 2 — One archive world

Scope: `archive_browser.rs` widget over `ArchiveLister`/`ArchiveScanRow`, dispatched on a `display_owner_site() -> SiteRef` shim (derived from `PollSource` until Phase 3); Unified Player Window/End-at gains the SMHI arm (via `archive_source()`); Event Loop Builder greys Build with the `ArchiveAccess::None { reason }` text when the display owner has no archive, seeds from the display owner, stops uppercasing intl input; one `IntlArchiveDataPack` (drop the K-prefix test assertion, data_packs.rs:259).

Gate: US, ORD, SMHI browse/load through the SAME widget (hour-grouped chips, loop-ending-at-scan, "+N older"); US archive flow pixel-identical (compare against Phase 1 behavior).
Owner checkpoint: load a Swedish archive day and a KTLX archive day from the same UI; build an event loop for `deess`; confirm honest grey with reason on DWD.

### Phase 3 — Site model everywhere (the type-system parity gate)

Scope: `sites_ui.rs`; all §1.4 consumers migrate; `site_is_primary_level2_catalog_site` and `site_is_tdwr` **deleted** (their logic becomes SiteKind classification inside the catalog module); `self.sites` and `intl_static_sites()` iteration confined by **privacy** (fields/functions private to the catalog modules) plus a CI altitude-guard test pinning the exact `self.sites` occurrence count in allowed files; pane SITE combos gain the grouped International section (the `pin: Option<SiteRef>` enum makes the Follow-primary no-op unrepresentable); startup restore for intl sessions.

Gate: settings round-trips (old bare favorites, old intl fields, old pinned ids all restore; case preserved for `:`-keys; forward-compat — v0.29 settings opened by v0.28 must not panic); altitude guard green; parity tripwires (intl favorite star works and survives restart; site search offers intl radars; event builder honest-grey; Reset View follows the intl session — already fixed at HEAD, pinned by test now). **After this phase a US-only feature cannot be written without an explicit match on SiteKind — the owner's constraint, satisfied here.**
Owner checkpoint: full Europe session — favorite Ängelholm, restart, resume lands in Sweden; right-click ranking shows intl rows with beam heights competing; 4-pane mixed US/intl with honest per-pane chips.

### Phase 4 — One loop engine (the risky one; strongest gates; 4-5 releases)

Ordered smallest-blast-radius first, each sub-stage its own release:

- **4a — RenderService step 1** (naming; `LaneId`; one budgeted drain for interactive+overlay lanes; prewarm drain moved verbatim). Zero behavior change.
- **4b — overlay pool flip** (its own commit; revertible alone). Gate: first-pixels ≤ the audited ~40 ms baseline; a 4-overlay storm scene shows same-or-better frame pacing than thread-per-overlay; loop recording still settles (media.rs settle-gating keys on textures). Test explicitly at LOW_CORE_PREVIEW_THREADS-class hardware.
- **4c — LoopEngine lands overlay-first.** Entry gate BEFORE any engine code merges: **behavior census** — read the FOUR install paths side by side (`install_decoded_load_batch` :8189, `install_extra_pane_decoded_load_batch` :14284, `install_polled_volume` :31088, `install_radar_layer_volume` :7747), table every micro-divergence (cursor-follow rules, `select_loaded_frame` timing, grow-limit-for-batch, live-partial deferral, status wording), decide keep-as-SelectionPolicy vs consciously-normalize per row. Overlays become `OverlayView{engine}`; overlay history joins the generation spine; `IntlOverlayFeed` dissolves; coordinated loops draw candidates from `sites_near()` with per-source window loaders (Mosaic near Copenhagen loads ORD sites; single-frame providers greyed "newest scan only").
- **4d — panes.** `PaneView{engine}`; pane tests become engine policy tests; SharedWithPrimary dedupe = SiteRef equality (intl double-poll dies). The pane context swap (`begin_extra_pane_context` :13832) is KEPT, swapping engine-owned fields — retiring it is a later cleanup, not part of this phase.
- **4e — primary.** Two sub-stages: (i) mechanical — the loose fields move into `self.primary: LoopEngine`, all legacy fns become field-path renames, compiler-driven, landed field-family by field-family; (ii) unify — legacy install/advance/liveness bodies become delegating shims onto engine methods, then get deleted. **Differential suite is the gate:** fixture histories (KEAX 2026-06-09 derecho ~40 frames; JMA merged repeated-elevation N5+N6; ORD REF-only mix) × sweep policies (Off/AllLow/BaseOnly/Range) × starting cursors, driving legacy fn and engine method side by side, asserting identical index/cut sequences over 3 full revolutions; same for install ordering and liveness truth tables. Only when green do shims delete. The US 1 s chunk-poll path stays **byte-identical** through this phase (live chunk timing is not fixture-testable; it is deliberately not unified into WorkerSlot until the engine is field-stable).

Gate (whole phase): differential suite green; chip truth-table now derives from `liveness()` (every (role, feed, live, age) cell asserted — the R8 class becomes untestable-to-reintroduce); pane double-poll dedupe test extended to intl; `HistoryLimits.byte_budget` enforced with tests; generation assertion harness active in debug builds.
Owner checkpoints: (4b) Mosaic 5 on a US site, spin the loop, drag-pan hard — no texture stutter; (4c) two overlays + a live intl overlay refreshing on cadence; (4d) 4-pane storm-day, one pane pinned to the primary's site says LIVE only when ITS poll is real, intl pane + intl primary same-site shows ONE download in diagnostics; (4e) KEAX 2026-06-09 derecho archive loop (loop feel, low-sweep stepping) + a live severe day; GO LIVE from an archive loop.

### Phase 5 — Simple front + deletion + payoff

Scope: `live_archive_bar.rs` — a **LIVE | ARCHIVE** segmented control in the top bar. LIVE = `set_feed(Live(current site))` + loop backfill via `loop_access()`; ARCHIVE = compact date/time popover → `FeedSource::Archive` via the Phase-2 browser. Entering either mode arms the sync defaults — the spine already exists and is audit-verified honest: `displayed_timeline_time_utc` (:9124), `surface_obs_frame_time_utc` (:9128), `arm_unified_player_timeline_warning_sync` (:9140), `sync_active_timeline_side_effects` (:9149). The **Unified Player becomes the "Advanced" disclosure of the bar** — its `UnifiedPlayerAction` dispatch (unified_player.rs:73, dispatch main.rs:~18828) is reused verbatim, not rebuilt. Every existing power control survives behind disclosure (Brand Kit precedent). Then: re-verified dead-inventory deletion (§ Read-this-first item 6); trust-label sweep (now enforceable — times from data, capability labels from derived catalog); fat-LTO CI attempt; guide self-tests updated; release notes crediting OPERA/national met services per policy.

Gate: guide self-tests green; UI walkthrough script; binary size + CI link comparison recorded; new settings default to old behavior and stay Eq-derivable.
Owner checkpoint: cold start → one click LIVE → storm-day loop with warnings/lightning/reports tracking the displayed frame → one click ARCHIVE → June 9 derecho loop with synced chrome → open Advanced and verify every old control still exists. Then hand the exe to a field tester with only the two buttons explained.

---

## 8. Migration table — load-bearing pieces (current → new home)

Anchors verified at 354a66b; grep the symbol name at implementation time.

| Current | Anchor | New home |
|---|---|---|
| `frame_history`/`selected_frame_index`/`history_playing`/`browsing_history`/`history_frame_limit` | main.rs:1623-1669 | `self.primary: LoopEngine` {history, cursor, limits} |
| `FrameHistory` + generation fn + `FrameHistoryEntry` + `FrameIdentity` | :2628-2757, :2821 | loop_engine.rs **VERBATIM** (bump discipline exactly; no DerefMut) |
| `RadarOverlayLayer.frame_history: Vec<FrameHistoryEntry>` | :2255 | `OverlayView.engine.history: FrameHistory` (overlays join the spine) |
| `texture`/`texture_key`/`pending_render_key` + render channels | :1690-1697 | `primary.tex: TextureSlot` + RenderService lane Primary |
| `loop_prewarm_receiver` + `spawn_loop_prewarm_render_workers` + `poll_loop_prewarm_renders` | :1697, :5255, :11850 | RenderService.prewarm — moved verbatim, drain NOT unified (inserts into loop_render_cache) |
| `poll_active`/`poll_last_file`/`poll_rx`/`poll_source` | :1926-1944 | `primary.feed` + `primary.poll: WorkerSlot` + `primary.live.dedupe_key`; `PollSource` (:4064) → FeedSource + settings shims |
| `realtime_level2_auto_refresh`/`last_realtime_level2_refresh` | :2150-2152 | `primary.live: LiveState`; ALL chrome reads via `engine.liveness()` |
| `ViewPane` | :4261-4306 | `PaneView { engine, pin: Option<SiteRef>, product, cut, map view }`; `pinned_site_id`+`intl_source` (:4268-4272) → `pin` |
| `PaneLiveSource`/`pane_live_poll_action`/`pane_live_poll_cadence_seconds` | :4362-4423 | `LiveAction`/`LoopEngine::live_tick`/`poll_cadence`; tests :63873+/:63935+ port verbatim, gain primary+overlay coverage |
| `extra_pane_live_source` | :7128 | `live_tick` with SiteRef equality (delivers intl SharedWithPrimary dedupe) |
| `follow_primary_volume_into_pane` + `followed_primary_volume_ptr` | :7231, :4283 | PaneView follow path; Arc-ptr dedupe unchanged |
| `IntlOverlayFeed` | :2221-2228 | dissolved into engine.feed / live.dedupe_key / poll |
| `install_polled_volume` / `install_decoded_load_batch` / `install_extra_pane_decoded_load_batch` / `install_radar_layer_volume` | :31088 / :8189 / :14284 / :7747 | `LoopEngine::install_frame`/`install_batch` + SelectionPolicy (behavior census first) |
| `advance_primary_screen_loop` / `advance_extra_pane_screen_loop` | :9655 / :9685 | `LoopEngine::advance_loop`; per-view side effects stay in ViewerApp keyed on StepOutcome |
| overlay timeline-sync frame selection | ~:7785-7817 | `engine.select_frame_nearest(t)` |
| `trim_frame_history` / `history_contains_other_site` | :8361 / :52024 | LoopEngine internals (+ NEW byte budget) |
| `pane_chip_is_live` / `mode_chip_state` family | :27748 / :36734-36790 | `engine.liveness()` + one chip renderer; INTL stale floor per FeedSource, `max(user, 1800, 2×cadence)` |
| `poll_async_render` / `poll_radar_layer_renders` | :11790 / :11895 | `RenderService::drain` + `LoopEngine::accept_render` |
| `spawn_render_worker`/`spawn_overlay_render_worker`/`spawn_render_worker_with_mode`, `merge_render_request`, `RenderRequest.pane` | :5239/:5247/:5336, :12735, :4438 | render_service.rs; `pane: usize` → `LaneId` |
| `poll_feed`/`drain_polled_volume`/`poll_source_armed`/`intl_poll_owns_primary`/`intl_source_owns_primary_display` | :31132-31206 | primary_feed.rs; ownership predicates become `primary.feed` inspections |
| `start_intl_poll` / `start_extra_pane_intl_load` / `fetch_intl_frame_plan_batch` | :31560 / :13941 / :51238 | primary_feed.rs / panes.rs over WorkerSlot/StreamSlot; plan-batch fetcher shared with archive_browser |
| `site_is_primary_level2_catalog_site` / `site_is_tdwr` | :42482 / :42491 | **DELETED**; SiteKind in data_source::sites (TJUA + TDWR = catalog data); callers :19172/:28435/:28537/:28777/:39647/:42521 + event_explorer.rs:258 become kind matches |
| `community_feed_for_site` | :42467 | data_source::sites (table already in community_feeds.rs) |
| `BeamTarget`/`BeamCandidate` | :304-325 | sites_ui.rs ranking over `sites_near()`; origin from SiteRecord.origin |
| favorites (bare level2 ids + uppercasing) | settings/src/lib.rs:765-776 | `SiteRef::settings_key`; `':'`-keys exempt from uppercase + case-sensitive compare |
| `RecentFrames`/`recent_source` | international.rs:140-208 | **UNCHANGED** (the pattern being copied); `ArchiveFrames`/`archive_source` added beside it; hand-maintained `archive_lookup` (:282) → derived + tripwire |
| `ord_archive_section` / `archive_panel` listing / SMHI coverage loaders | layers_rail.rs:2319-2528 / main.rs ~:14993-15188 / layers_rail.rs:2042-2083 | archive_browser.rs single widget over ArchiveLister/ArchiveScanRow |
| `level2_objects_for_date`/`_for_window` | data_source/src/lib.rs:511/:546 | unchanged; wrapped by `ArchiveLister::Us` (S3Object rows, NOT FramePlans) |
| `start_archive_window_load`/`start_archive_range_load_selecting`/`start_event_loop_archive_radar_load`/`start_ord_archive_window_load`/`start_smhi_coverage_archive_load` | :17040/:16885/:17186/:31727/:31245 | archive loads via ArchiveLister + StreamSlot, setting `FeedSource::Archive` (kills QW13 structurally) |
| `EventLoopRadarPlan.site_id: String` | event_loop_builder.rs:73 | `site: SiteRef`; Build dispatches on kind; uppercase-normalization removed for Intl |
| `UnifiedPlayerAction` dispatch | unified_player.rs:73, main.rs:~18828 | reused verbatim as the Advanced disclosure of the Phase-5 bar |

---

## 9. DO-NOT-TOUCH — verified-solid subsystems (both audits' lists)

Moved-not-modified is allowed where the table above says so; **no behavior edits** to:

- **Decode paths**: all of nexrad_io including odim.rs, jma.rs, the magic-byte router, the JMA N5+N6 merge + sort/renumber, the R1 spectrum-width fix.
- **Render worker internals**: `render_viewport_payload` + RenderWorkerCachePolicy/cache modes; the recycle-buffer machinery; loop-prewarm internals and its loop_render_cache contract.
- **Algorithm caches** keyed `(identity, volume-ptr)`; derived-product suites; the hail/wind algorithm suite; dealiasers.
- **The v0.28 FrameHistory generation newtype semantics** — build ON it; never weaken the bump discipline; `LoopTimelineSummaryCacheKey` keeps keying on `generation()`.
- **GLM/satellite machinery** (trailing-window sync, scan-start timestamps, LRU texture budget) and grid composites' honest-exclusion model.
- **media.rs** settle-gating and deterministic 1× export cadence.
- **The updater / version check** (`self_update_rx` migrates to WorkerSlot mechanically; logic untouched).
- **sharprs**, tiles.rs anti-shear machinery, the AEQD rotation fix.
- **The US realtime chunk chain** (barrier-free downloads, KLNX dirty grouping, live-partial guards): byte-identical through Phase 4; `LiveService`-style unification of chunk polling is explicitly OUT of v0.29.
- **The shared US Level-II archive loader** (00Z boundaries, anchor selection): wrapped by `ArchiveLister::Us`, never re-expressed as FramePlans.

---

## 10. Risks and mitigations

1. **Payoff inversion (the chosen sequencing's own weakness).** Identity/archive plumbing lands for three phases before the engine consolidates; three engines coexist until Phase 4. Mitigation: honest-grey and capability-card fixes ship in Phases 1-2 so trust does not wait on architecture; every phase has an owner-felt deliverable; if Phase 4e (primary port) slips, Phases 1-4d still ship a coherent v0.29 and the Phase-5 bar can drive the OLD primary plumbing through the same intents.
2. **SelectionPolicy rot / silent normalization.** The three loop copies are drifting mirrors, not twins. Mitigation: the Phase 4c behavior census is a **blocking deliverable** (a table in the PR description, one row per divergence, keep/normalize decision each); the differential suite only covers fixtured behavior — the census is what decides what gets fixtured.
3. **Phase 4e is a big-bang in disguise if done carelessly.** Thousands of field reads touch the primary loop fields; `&mut self` borrow-order changes are subtle. Mitigation: mechanical sub-stage (i) is a compiler-driven field move with zero logic change, landed field-family by field-family; only sub-stage (ii) changes code paths, behind the differential gate; delegation accessors (`fn frame_history(&self) -> &FrameHistory`) keep the diff mechanical.
4. **The pane context swap** (`begin_extra_pane_context` :13832 swaps `self.volume`; dozens of UI fns read the swapped volume). Retiring it doubles Phase 4d's blast radius. Decision: **keep the swap**, swapping engine-owned fields; retire in a post-v0.29 cleanup. Honest cost: LoopEngine ownership stays slightly compromised through Phase 5.
5. **Overlay render-pool flip is a real scheduling change** on low-core machines. Mitigation: it is its own commit (revert = one revert), gated on first-pixels ≤ ~40 ms + 4-overlay pacing telemetry, pool size K is a diagnostics-visible constant escape hatch, and the interactive worker stays separate so the primary can never be starved. Test at LOW_CORE_PREVIEW_THREADS-class hardware before the gate is called.
6. **Settings/persistence compat.** Every legacy encoding must round-trip: bare favorites (uppercased), `intl_provider`/`intl_site`, `poll_url`, pinned ids; `':'`-keys case-preserved; forward-compat (v0.29 file opened by v0.28 must not panic — new keys only, no repurposed keys). Mandatory tests in Phases 1 and 3; AppSettings stays Eq-derivable (strings only, no f32, never serde the enum).
7. **Write-fleet drift.** main.rs grew 67,364→70,600 across the two audits; c1e243c moved parity-region lines by hundreds. Rules: symbol-anchored work orders; extraction slices land within days; a rebase-heavy branch is a smell the slice is too big; re-grep every anchor at implementation start.
8. **Generation discipline under new engine methods.** A skipped bump = stale loop caches; bump-on-read = thrash. Mitigation: the debug assertion harness (§3) plus the existing generation-keyed cache tests.
9. **Differential tests prove fixtures, not live timing.** The 1 s chunk cadence + live-partial substitution interplay is time-dependent. Mitigation: the chunk path stays byte-identical through Phase 4; at least one live severe-weather owner session per phase gate.
10. **ArchiveFrames over-promising for awkward upstreams** (FMI 2007-present walk, NCI tarlists, DMI STAC paging). Mitigation: the trait's cheapness contract (catalog probes only) + per-provider `window_plans` overrides; ship ORD/SMHI/US only in v0.29; the tripwire-tested id set grows deliberately, exactly as `recent_source()` did.
11. **CI linker OOM before extractions relieve it.** If it bites early, pull the Phase 5 dead-inventory re-verification + deletion forward — it is independent by construction.

**Named non-goals for v0.29** (deferred, written down so scope pressure has an answer): per-pane model-layer renders (parity structural E — the multi-pane composite vanish keeps its interim honesty note); composite time-series loops (DPC/CWA are honestly latest-only layers); deep FMI/NCI/DMI/JMA archive UIs beyond the trait hook; migrating all ~42 worker slots; retiring the pane context swap; any streaming intl feed abstraction; tokio or any async runtime.

---

## 11. Phase-1 work order (hand to implementation agents verbatim)

**Branch:** off `v028/unslop` @ HEAD. **Ground rules:** every milestone is its own PR-sized commit series; extraction/addition commits never mix with behavior changes; `cargo test --workspace` + `cargo fmt --check` + `cargo clippy --workspace -D warnings` green at every commit; re-grep every line anchor before editing (they drift); do not touch anything in §9. When a task says "verbatim", `git diff` must show pure moves.

### Milestone A — contract harness (tests only; no production changes)

New tests in the existing `mod tests` of main.rs (or a sibling test module) pinning CURRENT behavior:

1. **Chip truth table.** For `mode_chip_state_with_live_and_stale_floor` (main.rs:36765): assert (live=true, age≤floor) → LIVE; (live=true, age>max(user,floor)) → LIVE·STALE; (live=false, age<24h) → "ARCHIVE · Nm old"; (live=false, age≥24h) → dated ARCHIVE, for floor=0 and floor=1800. For `mode_chip_state` (:36734): intl display owner routes through the floor variant with `intl_poll_owns_primary()` as liveness. For `pane_chip_is_live` (:27748): independent pane = pane.live && source≠None; **pane 0 falls through to `realtime_level2_auto_refresh` — pin the CURRENT (known-wrong) behavior with a comment naming it as the R8 residue Phase 4 fixes.**
2. **History-clear rules.** Table-test the observable clear/keep behavior for: `start_intl_poll` same-site (keep) vs cross-site/cross-source (clear); `install_polled_volume` cross-site (clear via `history_contains_other_site` :52024); `start_extra_pane_intl_load` pane clears. Reuse/extend the existing tests around :63873-64000 rather than duplicating.
3. **Pane policy family.** Extend `pane_live_poll_action_policy` (:63873) and `extra_pane_live_source_dedupes_primary_site_and_rejects_foreign_ids` (:63935) with any uncovered (source, primary-state) cells you find in `pane_live_poll_action` (:4404) / `extra_pane_live_source` (:7128).
4. **Settings round-trips.** v0.28-shaped JSON with: bare favorites (mixed case in → uppercased out, current behavior), `intl_provider`/`intl_site`, `poll_url`, `startup_site` — save/load/compare. These pin the encodings Milestone C must not break.

Gate A: suite green; zero production diffs.

### Milestone B — `crates/app_ui/src/worker_slot.rs`

1. Implement `WorkerSlot<T>`, `WorkerTx<T>`, `StreamSlot<T: SlotMessage>` exactly per §4.1 (drop-rx cancellation; send = send + `request_repaint`; `poll()` never blocks; Ready/Disconnected clears rx; `label` + `started` exposed for diagnostics). Unit tests: spawn-while-in-flight returns false; cancel-then-worker-send is a clean no-op; Disconnected clears the slot; StreamSlot stays busy until `is_terminal`.
2. Migrate these ten slots in 2-3 commits, mechanical, status strings greppable-identical: `update_check_rx` (:2171), `self_update_rx` (:2177), `intl_sites_rx` (:1951), `coverage_probe_rx` (:1959), `ord_archive_list_rx` (:1968), `italy_dpc_latest_rx` (:1637), `taiwan_cwa_latest_rx` (:1642), `radar_operational_status_rx` (:1813), `spc_receiver` (:1816), `upper_air_rx` (:1887). Do NOT touch `poll_rx`, `intl_loop_rx`, or any `load_receiver` — those are Phase 4.
3. While migrating the Taiwan slot: the job must stop writing the global status bar (return the string; owner writes it at drain) — this is the ONE permitted behavior change in Phase 1, and it gets its own commit + a test asserting the status text still appears (now written by the drain site).

Gate B: suite green; migrated features behave identically (update check, intl site list, coverage probe, Italy/Taiwan latest, SPC reports, upper air).

### Milestone C — site + archive foundations (additive; no consumer migration)

1. **`crates/data_source/src/sites.rs`** (new): `SiteRef`, `SiteKind`, `SiteRecord`, `settings_key`/`parse_settings_key`, `resolve`, `all_sites`, `sites_near` per §1. US data from the embedded catalog (`RadarSite`, lib.rs:72) + `community_feeds` table; intl from `intl_static_sites()` (international.rs:257). TJUA classifies `Wsr88d`; TDWR classification is catalog-data-driven (port the current id list; the `'T'`-prefix heuristic may be used ONLY inside this module to seed the data, with a test asserting JMA TAKA/TANE/TOJI classify `Intl`). Tests: round-trip every variant through settings_key; bare-string parse = Us; case preservation for intl; `sites_near` distance ordering with a US/intl interleave; every `intl_static_sites` entry resolves.
2. **`ArchiveFrames` + `archive_source()`** in international.rs per §1.2, beside `recent_source()` (:200), same doc style. `OrdProvider` impl wrapping `archive_plans_for_hour` (ord.rs:313); `SmhiProvider` impl wrapping `smhi_archive_plans_for_day` (smhi.rs:72), oldest-first, catalog-probes-only. Flip the capability struct's `archive_lookup` (international.rs:282) to derive from `supports_archive()`; update the SMHI/NCI card text accordingly (NCI has no archive_source yet → its card goes honest-false). **Tripwire test** mirroring `recent_source_routes_recent_and_flips_supports_recent_together` (:1012): the archive-capable id set is exactly `{"ord","smhi"}`.
3. **Settings uppercasing fix** (settings/src/lib.rs:765-776): keys containing `':'` are pushed verbatim and compared case-sensitively in `add_favorite`/`remove_favorite`/`is_favorite`; bare keys keep exact current behavior. Tests: `"intl:ord:deess"` survives add/is/remove with case intact; `"ktlx"` still uppercases to `"KTLX"`; the Milestone-A round-trip tests still pass unchanged.
4. **Grep-guard test** (app_ui): assert the file list containing `level2_objects_for_date`/`_for_window` callers matches a pinned set, so no new direct callers land while the archive world unifies in Phase 2.
5. NO app_ui consumer migrates to `SiteRef` in this phase (that is Phase 3). The only user-visible change in all of Phase 1 is the honest capability cards.

Gate C: full workspace suite green (count strictly greater than at branch point); fmt + clippy clean; tripwires green.

### Owner checkpoint (build release-fast exe locally)

1. Live storm session: load latest on a US site, Load Loop, live poll an intl site (e.g. `deess`), archive-load an ORD day — everything identical to v0.28.2.
2. Data tab → Radar coverage: SMHI card now shows Archive=true; NCI shows Archive=false with its next-unlock text; all other cards unchanged.
3. Favorites: star a US site, restart, chip works — unchanged.
4. Update check + Italy DPC + Taiwan CWA layers refresh normally (WorkerSlot migrations invisible).

Deliverable: tag `v0.29.0-alpha.1` locally. Report: test-count delta, any anchor drift found (with corrected lines), and the Phase-4 behavior-census candidates you noticed in passing (do not act on them).

---

## 12. Owner decisions (resolved 2026-07-02, pre-Phase-1)

1. **Overlay render-pool flip (Phase 4b): ATTEMPT IT, gated** — its own
   individually-revertible commit, survives only if first-pixels ≤ ~40 ms
   and 4-overlay pacing telemetry stay clean.
2. **Phase 4e fallback: partial end state IS releasable as v0.29.0** —
   overlays + panes on the engine with the two-button bar driving the old
   primary plumbing through the same intents; the primary port lands in
   v0.29.1 when it passes its gate.
3. **Intl staleness: CADENCE-AWARE floor** — `max(user, 1800 s, 2×poll
   cadence)` per FeedSource, landing with `engine.liveness()` in Phase 4
   (supersedes the flat v0.28.2 constant).

Release process for the program: each phase checkpoint = locally built
release-fast exe for the owner (no GitHub releases); v0.29.0 is the next
public tag.
