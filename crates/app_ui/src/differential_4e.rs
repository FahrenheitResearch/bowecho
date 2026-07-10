//! Phase 4e stage (ii) — THE DIFFERENTIAL SUITE (spec §7 Phase 4e bullet,
//! `docs/v029-engine-spec.md`). This module is the GATE that decides whether
//! the legacy primary install/advance/liveness bodies may become delegating
//! shims onto `ui_core::loop_engine` methods: fixture histories × sweep
//! policies × starting cursors drive the LEGACY functions and the ENGINE
//! methods side by side, asserting identical index/cut sequences over three
//! full loop revolutions; the same treatment covers install ordering and the
//! liveness (mode-chip) truth tables. Only when this suite is green do the
//! shims land; the US 1 s chunk-poll chain stays byte-identical regardless
//! (spec §9) — live chunk timing is deliberately NOT fixture-testable.
//!
//! Behavior authority: docs/v029-phase4-behavior-census.md (D1–D16, all
//! BINDING). Every deliberate difference between the two sides cites its
//! Decision row; every test that pins a divergence names its class:
//!
//! - class (a) — engine bug: the ENGINE gets fixed, never the legacy side.
//! - class (b) — census/spec-decided normalize: asserted as the DECIDED
//!   shape, with the D-number (or spec §) in the comment.
//! - class (c) — genuine divergence awaiting an owner decision: the cell is
//!   left RED under `#[ignore = "…"]` so `cargo test -- --ignored` shows it;
//!   the unify must not proceed through that cell until decided.
//!
//! Suite shape:
//! - advance — 3 fixtures × 5 sweep policies (Off / AllLow / BaseOnly /
//!   SameLevel / Range — the gate names four; SameLevel is the fifth real
//!   legacy mode and is covered for completeness) × 4 starting cursors
//!   (first / middle / newest / beyond-trim): the real
//!   `ViewerApp::advance_primary_screen_loop` vs `LoopEngine::advance_loop`
//!   under the stage-(ii) caller shape, plus pure-fn edge cells.
//! - install — legacy `install_decoded_load_batch` (census P1) vs
//!   `install_batch` + `SelectionPolicy::SelectAnchor`/`Backfill`; legacy
//!   `install_polled_volume` (P3) vs `install_frame` +
//!   `FollowNewestUnlessPlaying`; ordering, guards, deferral, cross-site
//!   clears, grow-limit intent, trims.
//! - liveness — the `mode_chip_state` family vs a chip renderer derived
//!   from `LoopEngine::liveness()` for every (role, feed, live, age) cell
//!   that exists on both sides (the R8 truth table).
//!
//! The engine-side "mirror" in each test is the code the stage-(ii) unify
//! will install in ViewerApp: display writes, status strings and the
//! within-frame low-sweep walk stay CALLER-side (census D6/D11, engine
//! advance.rs module doc), so the mirror reproduces exactly that caller.
//!
//! STAGE (iii) STATUS (the unify this suite gated): the GREEN cells
//! landed — `install_decoded_load_batch` (P1), both screen-loop steppers,
//! the primary trims, and the `mode_chip_state` family are delegating
//! wrappers over the engine, so the "legacy" side of those tests now
//! drives the unified path and each differential doubles as a
//! wiring-fidelity pin. The two class-(c) cells below are still RED and
//! `install_polled_volume` (P3) therefore remains a true legacy body —
//! its differentials still compare two genuinely different
//! implementations. Class-(b) pins were flipped to the landed normalize
//! where the stage-(iii) commit made them true (empty-strip stepping,
//! newest-frame chip age, the pane-0 R8 residue).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use data_source::sites::SiteRef;
use eframe::egui;
use radar_core::{ElevationCut, MomentGrid, MomentType, RadarVolume};
use settings::{LoopSweepControl, SweepPolicy, SweepPolicyMode, SweepPolicySet, SweepProductGroup};
use ui_core::loop_engine::{InstallOutcome, StepOutcome, SweepContext};

use crate::tests::{
    test_pane, test_velocity_sails_volume_with_radials, test_viewer_app_with_hazards,
};
use crate::{
    DecodedLoad, DecodedLoadBatch, DisplayProduct, EngineId, EngineRole, FeedSource, FrameHistory,
    FrameHistoryEntry, FrameStatus, InstallSelection, Liveness, LoadTimings, LoopEngine,
    LowSweepCutKey, SelectionPolicy, ViewerApp, frame_identity_for_volume,
    should_defer_live_partial_selection_for_active_product, sweep_cuts_for_history_entry,
};

/// The primary-role anchor-select policy (census D5b: the blank-display
/// escape is Primary-only).
const SELECT_PRIMARY: SelectionPolicy = SelectionPolicy::SelectAnchor {
    blank_display_overrides_browsing: true,
};

fn ref_product() -> DisplayProduct {
    DisplayProduct::Moment(MomentType::Reflectivity)
}

// ---------------------------------------------------------------------------
// Fixtures — synthesized histories with realistic cut/elevation/time
// structure (metadata realism, not real decoding).
// ---------------------------------------------------------------------------

/// Build one fixture volume: `cuts` = (elevation°, first-radial offset ms
/// from volume time, radial count). 720 radials = a complete 360° tilt
/// (matches `LIVE_COMPLETE_LOW_LEVEL_TILT_MIN_RADIALS`); fewer radials = a
/// partial sector, exactly how a mid-scan live cut looks. Cut start times
/// are strictly increasing so at-or-before cut selection is exact.
fn fixture_volume(site: &str, scan_time: DateTime<Utc>, cuts: &[(f32, i32, usize)]) -> RadarVolume {
    let gate_range = radar_core::GateRange {
        first_gate_m: 500,
        gate_spacing_m: 250,
        gate_count: 3,
    };
    let mut volume = RadarVolume::new(radar_core::RadarSite::new(site), scan_time);
    for (cut_number, (elevation_deg, time_offset_ms, radial_count)) in cuts.iter().enumerate() {
        let mut cut = ElevationCut::new(*elevation_deg, Some(cut_number as u8));
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            2.0,
            66.0,
            Some(0),
            Some(1),
        );
        for row in 0..*radial_count {
            cut.radials.push(radar_core::Radial {
                azimuth_deg: row as f32,
                elevation_deg: *elevation_deg,
                time_offset_ms: *time_offset_ms + row as i32,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(32.0),
                radial_status: None,
            });
            grid.push_u8_row_slice(row, &[66, 80, 90]).expect("ref row");
        }
        cut.moments.insert(MomentType::Reflectivity, grid);
        volume.cuts.push(cut);
    }
    volume
}

fn fixture_entry(
    site: &str,
    scan_time: DateTime<Utc>,
    cuts: &[(f32, i32, usize)],
    status: FrameStatus,
    path: &str,
) -> FrameHistoryEntry {
    let volume = Arc::new(fixture_volume(site, scan_time, cuts));
    FrameHistoryEntry {
        identity: frame_identity_for_volume(&volume),
        path: PathBuf::from(path),
        volume,
        timings: None,
        status,
        source_label: format!("fixture {site}"),
    }
}

/// One advance fixture: a site, a frame strip, and the Range policy that is
/// meaningful for its elevation menu.
struct AdvanceFixture {
    name: &'static str,
    site: &'static str,
    frames: Vec<FrameHistoryEntry>,
    range_policy: SweepPolicy,
}

/// (a) KEAX 2026-06-09 derecho-class: 40 frames on a VCP-212-like cadence
/// (~4.5 min), 8 cuts with SAILS 0.5° inserts, the last two frames
/// live-partial (later tilts incomplete → fewer steppable cuts; the final
/// frame has NO complete tilt inside the Range band, so Range-mode stepping
/// must skip it).
fn keax_derecho_fixture() -> AdvanceFixture {
    let base = Utc.with_ymd_and_hms(2026, 6, 9, 3, 0, 0).unwrap();
    let complete_cuts: Vec<(f32, i32, usize)> = vec![
        (0.5, 0, 720),
        (0.9, 26_000, 720),
        (1.3, 52_000, 720),
        (0.5, 78_000, 720), // SAILS 1
        (1.8, 104_000, 720),
        (2.4, 130_000, 720),
        (0.5, 156_000, 720), // SAILS 2 (MESO-SAILS ×2)
        (3.1, 182_000, 720),
    ];
    let mid_volume_partial: Vec<(f32, i32, usize)> = vec![
        (0.5, 0, 720),
        (0.9, 26_000, 720),
        (1.3, 52_000, 720),
        (0.5, 78_000, 720),
        (1.8, 104_000, 300), // partial sector — incomplete
        (2.4, 130_000, 80),
        (0.5, 156_000, 80),
        (3.1, 182_000, 80),
    ];
    let early_partial: Vec<(f32, i32, usize)> = vec![
        (0.5, 0, 720), // only the base tilt is complete
        (0.9, 26_000, 300),
        (1.3, 52_000, 80),
        (0.5, 78_000, 80),
        (1.8, 104_000, 80),
        (2.4, 130_000, 80),
        (0.5, 156_000, 80),
        (3.1, 182_000, 80),
    ];
    let mut frames = Vec::new();
    for index in 0..40usize {
        let scan_time = base + ChronoDuration::seconds(270 * index as i64);
        let (cuts, status) = match index {
            38 => (&mid_volume_partial, FrameStatus::LivePartial),
            39 => (&early_partial, FrameStatus::LivePartial),
            _ => (&complete_cuts, FrameStatus::Complete),
        };
        frames.push(fixture_entry(
            "KEAX",
            scan_time,
            cuts,
            status,
            &format!("keax-derecho-{index:02}"),
        ));
    }
    AdvanceFixture {
        name: "keax-derecho",
        site: "KEAX",
        frames,
        // 0.8°..=2.0°: brackets 0.9/1.3/1.8, excludes the SAILS base tilts.
        range_policy: SweepPolicy::range_cdeg(80, 200),
    }
}

/// (b) JMA-class: merged repeated-elevation volumes (the N5+N6 duplicate-
/// elevation shape — each elevation appears twice per merged volume with
/// distinct collection times). Site id "TANE" keeps the known T-prefix
/// terminal-radar heuristic in play IDENTICALLY on both sides.
fn jma_merged_fixture() -> AdvanceFixture {
    let base = Utc.with_ymd_and_hms(2026, 6, 9, 4, 0, 0).unwrap();
    let cuts: Vec<(f32, i32, usize)> = vec![
        (0.7, 0, 720),
        (1.1, 30_000, 720),
        (2.6, 60_000, 720),
        (0.7, 90_000, 720), // second file of the merge repeats the ladder
        (1.1, 120_000, 720),
        (2.6, 150_000, 720),
    ];
    // Slightly uneven 5/5/10-minute cadence.
    let gaps_min = [0i64, 5, 10, 20, 25, 30, 40, 45];
    let frames = gaps_min
        .iter()
        .enumerate()
        .map(|(index, minutes)| {
            fixture_entry(
                "TANE",
                base + ChronoDuration::minutes(*minutes),
                &cuts,
                FrameStatus::Complete,
                &format!("jma-merged-{index:02}"),
            )
        })
        .collect();
    AdvanceFixture {
        name: "jma-merged",
        site: "TANE",
        frames,
        // 0.9°..=1.2°: exactly the duplicated 1.1° pair.
        range_policy: SweepPolicy::range_cdeg(90, 120),
    }
}

/// (c) ORD REF-only mix: reflectivity-only frames (no velocity anywhere —
/// the missing-moment dimension is exercised by the pure-fn velocity cell
/// below) on an uneven 5/10/15-minute cadence.
fn ord_ref_only_fixture() -> AdvanceFixture {
    let base = Utc.with_ymd_and_hms(2026, 6, 9, 5, 0, 0).unwrap();
    let cuts: Vec<(f32, i32, usize)> = vec![(0.5, 0, 720), (1.5, 40_000, 720), (3.5, 80_000, 720)];
    let gaps_min = [0i64, 5, 10, 20, 25, 40, 45, 50, 60, 65];
    let frames = gaps_min
        .iter()
        .enumerate()
        .map(|(index, minutes)| {
            fixture_entry(
                "deess",
                base + ChronoDuration::minutes(*minutes),
                &cuts,
                FrameStatus::Complete,
                &format!("ord-ref-{index:02}"),
            )
        })
        .collect();
    AdvanceFixture {
        name: "ord-ref-only",
        site: "deess",
        frames,
        // 1.0°..=2.0°: only the 1.5° tilt.
        range_policy: SweepPolicy::range_cdeg(100, 200),
    }
}

// ---------------------------------------------------------------------------
// Advance differential — legacy `advance_primary_screen_loop` vs
// `LoopEngine::advance_loop` under the stage-(ii) caller shape.
// ---------------------------------------------------------------------------

/// The five real legacy sweep modes. "Off" is the user toggle
/// (`loop_low_sweeps = false`) — the machinery off, plain modulo stepping.
#[derive(Clone, Copy, Debug)]
enum PolicyCell {
    Off,
    AllLow,
    BaseOnly,
    SameLevel,
    Range,
}

const POLICY_CELLS: [PolicyCell; 5] = [
    PolicyCell::Off,
    PolicyCell::AllLow,
    PolicyCell::BaseOnly,
    PolicyCell::SameLevel,
    PolicyCell::Range,
];

fn cell_policy(cell: PolicyCell, range_policy: SweepPolicy) -> Option<SweepPolicy> {
    let mode = match cell {
        PolicyCell::Off => return None,
        PolicyCell::AllLow => SweepPolicyMode::AllLow,
        PolicyCell::BaseOnly => SweepPolicyMode::BaseOnly,
        PolicyCell::SameLevel => SweepPolicyMode::SameLevel,
        PolicyCell::Range => return Some(range_policy),
    };
    Some(SweepPolicy {
        mode,
        ..SweepPolicy::default()
    })
}

fn sweep_control_for(policy: SweepPolicy) -> LoopSweepControl {
    let mut product_groups = std::collections::BTreeMap::new();
    for group in SweepProductGroup::ALL {
        product_groups.insert(group, policy);
    }
    LoopSweepControl {
        primary: SweepPolicySet { product_groups },
        extra_pane_overrides: std::collections::BTreeMap::new(),
    }
}

/// One observed playback tick: (frame index, selected cut, still playing).
type Tick = (usize, usize, bool);

/// Drive the REAL legacy stepper. Cursor/cut/playing are read back from the
/// app after each call — `set_shared_low_sweep_time` writes `selected_cut`
/// synchronously, so the whole tick is observable without textures.
fn drive_legacy_advance(app: &mut ViewerApp, ctx: &egui::Context, max_ticks: usize) -> Vec<Tick> {
    let mut ticks = Vec::new();
    for _ in 0..max_ticks {
        app.advance_primary_screen_loop(ctx);
        ticks.push((
            app.primary.cursor.index,
            app.selected_cut,
            app.primary.cursor.playing,
        ));
        if !app.primary.cursor.playing {
            break;
        }
    }
    ticks
}

/// The legacy `primary_history_loop_can_step` over engine-owned state (the
/// stage-(ii) caller re-evaluates `playing` after every step — engine
/// advance.rs module doc). Range mode replicates the loop-timeline summary
/// step count (`cut_count == 0 && Range → 0, else max(1, cut_count)`).
fn mirror_can_step(
    history: &FrameHistory,
    cursor_index: usize,
    product: &DisplayProduct,
    policy: Option<SweepPolicy>,
    disabled: &BTreeSet<LowSweepCutKey>,
) -> bool {
    if let Some(policy) = policy {
        if policy.mode == SweepPolicyMode::Range {
            let mut step_count = 0usize;
            for frame in history.iter() {
                let cut_count =
                    sweep_cuts_for_history_entry(frame, product, policy, disabled).len();
                if cut_count > 0 {
                    step_count += cut_count.max(1);
                }
            }
            return step_count > 1;
        }
        if history.len() > 1 {
            return true;
        }
        let Some(frame) = history.get(cursor_index) else {
            return false;
        };
        return sweep_cuts_for_history_entry(frame, product, policy, disabled).len() > 1;
    }
    history.len() > 1
}

/// Drive the ENGINE under the stage-(ii) caller shape: the within-frame
/// low-sweep walk runs caller-side and short-circuits the engine step (it
/// moves the cut, not the cursor); `advance_loop` owns the frame-index
/// math; entering a frame snaps to its first steppable cut (the legacy
/// `select_first_history_low_sweep_if_enabled`); `playing` is re-evaluated
/// after each step.
fn drive_engine_advance(
    engine: &mut LoopEngine,
    product: &DisplayProduct,
    policy: Option<SweepPolicy>,
    max_ticks: usize,
) -> Vec<Tick> {
    let disabled: BTreeSet<LowSweepCutKey> = BTreeSet::new();
    let normalized = policy.map(SweepPolicy::normalized);
    let range_mode = normalized.is_some_and(|policy| policy.mode == SweepPolicyMode::Range);
    // The strip never mutates during playback, so the can-step verdict is
    // constant for the whole drive (the legacy caller reaches the same
    // conclusion through its generation-keyed summary cache) — computed
    // once instead of per tick.
    let can_step = mirror_can_step(
        &engine.history,
        engine.cursor.index,
        product,
        normalized,
        &disabled,
    );
    let mut cut = 0usize;
    let mut ticks = Vec::new();
    for _ in 0..max_ticks {
        // Within-frame walk (caller-side, before the engine step).
        if let Some(policy) = normalized
            && let Some(frame) = engine.history.get(engine.cursor.index)
        {
            let cuts = sweep_cuts_for_history_entry(frame, product, policy, &disabled);
            match cuts.iter().position(|candidate| *candidate == cut) {
                Some(position) => {
                    if let Some(next) = cuts.get(position + 1) {
                        cut = *next;
                        ticks.push((engine.cursor.index, cut, true));
                        continue;
                    }
                    // Last cut: fall through to the frame advance.
                }
                None => {
                    if let Some(first) = cuts.first() {
                        // Legacy `set_shared_low_sweep_rank(0)`: a cursor cut
                        // outside the steppable list snaps to the first cut
                        // and consumes the tick.
                        cut = *first;
                        ticks.push((engine.cursor.index, cut, true));
                        continue;
                    }
                    // No steppable cuts: fall through to the frame advance.
                }
            }
        }
        let has_cuts = |frame: &FrameHistoryEntry| -> bool {
            normalized.is_some_and(|policy| {
                !sweep_cuts_for_history_entry(frame, product, policy, &disabled).is_empty()
            })
        };
        let sweep = SweepContext {
            range_mode,
            frame_has_cuts: &has_cuts,
        };
        match engine.advance_loop(&sweep) {
            StepOutcome::Stopped => {
                ticks.push((engine.cursor.index, cut, false));
                break;
            }
            StepOutcome::Stepped { index } => {
                if let Some(policy) = normalized
                    && let Some(frame) = engine.history.get(index)
                    && let Some(first) =
                        sweep_cuts_for_history_entry(frame, product, policy, &disabled).first()
                {
                    // Entering a frame selects its first steppable cut
                    // (legacy `select_first_history_low_sweep_if_enabled`).
                    cut = *first;
                }
                engine.cursor.playing = can_step;
                ticks.push((index, cut, engine.cursor.playing));
                if !engine.cursor.playing {
                    break;
                }
            }
        }
    }
    ticks
}

/// Ticks for ~3 revolutions of a fixture under a policy (each visited frame
/// consumes one tick per steppable cut, one tick when it has none, zero
/// when Range skips it), plus slack for the partial first revolution.
fn ticks_for_three_revolutions(
    fixture: &AdvanceFixture,
    product: &DisplayProduct,
    policy: Option<SweepPolicy>,
) -> usize {
    let disabled: BTreeSet<LowSweepCutKey> = BTreeSet::new();
    let per_revolution: usize = fixture
        .frames
        .iter()
        .map(|frame| match policy.map(SweepPolicy::normalized) {
            Some(policy) => {
                let cuts = sweep_cuts_for_history_entry(frame, product, policy, &disabled).len();
                if policy.mode == SweepPolicyMode::Range {
                    cuts
                } else {
                    cuts.max(1)
                }
            }
            None => 1,
        })
        .sum();
    3 * per_revolution.max(1) + 8
}

fn run_advance_fixture_cells(fixture: &AdvanceFixture) {
    let ctx = egui::Context::default();
    let product = ref_product();
    let frame_count = fixture.frames.len();
    // first / middle / newest / beyond-trim (a raw post-trim cursor one past
    // the end, before any clamp).
    let starts = [0usize, frame_count / 2, frame_count - 1, frame_count];
    for policy_cell in POLICY_CELLS {
        let policy = cell_policy(policy_cell, fixture.range_policy);
        let max_ticks = ticks_for_three_revolutions(fixture, &product, policy);
        for start in starts {
            let label = format!("{} / {policy_cell:?} / start {start}", fixture.name);

            // Legacy side: a real ViewerApp driving the real stepper.
            let mut app = test_viewer_app_with_hazards(Vec::new());
            for frame in &fixture.frames {
                app.primary.history.push(frame.clone());
            }
            app.primary.cursor.index = start;
            app.primary.cursor.playing = true;
            app.selected_cut = 0;
            let display_index = start.min(frame_count - 1);
            app.volume = Some(Arc::clone(&fixture.frames[display_index].volume));
            match policy {
                Some(policy) => {
                    app.app_settings.loop_low_sweeps = true;
                    app.app_settings.loop_sweep_control = Some(sweep_control_for(policy));
                }
                None => app.app_settings.loop_low_sweeps = false,
            }
            let legacy = drive_legacy_advance(&mut app, &ctx, max_ticks);

            // Engine side: the stage-(ii) caller over LoopEngine::advance_loop.
            let mut engine = LoopEngine::new(
                EngineId(41),
                EngineRole::Primary,
                FeedSource::Live(SiteRef::Us {
                    level2_id: fixture.site.to_owned(),
                }),
            );
            engine.history = FrameHistory::from(fixture.frames.clone());
            engine.cursor.index = start;
            engine.cursor.playing = true;
            let mirror = drive_engine_advance(&mut engine, &product, policy, max_ticks);

            assert_eq!(
                legacy, mirror,
                "index/cut/playing sequences must be identical: {label}"
            );
            assert_eq!(
                app.primary.cursor.index, engine.cursor.index,
                "final cursor: {label}"
            );
            assert_eq!(
                app.primary.cursor.playing, engine.cursor.playing,
                "final playing flag: {label}"
            );
        }
    }
}

#[test]
fn advance_diff_keax_derecho_all_policies_and_cursors_match_engine() {
    run_advance_fixture_cells(&keax_derecho_fixture());
}

#[test]
fn advance_diff_jma_merged_elevations_all_policies_and_cursors_match_engine() {
    run_advance_fixture_cells(&jma_merged_fixture());
}

#[test]
fn advance_diff_ord_ref_only_all_policies_and_cursors_match_engine() {
    run_advance_fixture_cells(&ord_ref_only_fixture());
}

/// The missing-moment dimension of the ORD fixture: a velocity product over
/// a REF-only history leaves NOTHING steppable in Range mode — the engine
/// stepper must stop and clear `playing`. (Until stage (iii) this cell also
/// drove the legacy pure fn `next_frame_index_with_sweep_cuts` side by
/// side; that fn is deleted — `advance_loop` is the only stepper.)
#[test]
fn advance_diff_velocity_over_ref_only_history_stops_both_sides() {
    let fixture = ord_ref_only_fixture();
    let velocity = DisplayProduct::Moment(MomentType::Velocity);
    let policy = fixture.range_policy.normalized();
    let disabled: BTreeSet<LowSweepCutKey> = BTreeSet::new();

    let mut engine = LoopEngine::new(
        EngineId(42),
        EngineRole::Primary,
        FeedSource::Live(SiteRef::Us {
            level2_id: "deess".to_owned(),
        }),
    );
    engine.history = FrameHistory::from(fixture.frames.clone());
    engine.cursor.playing = true;
    let has_cuts = |frame: &FrameHistoryEntry| -> bool {
        !sweep_cuts_for_history_entry(frame, &velocity, policy, &disabled).is_empty()
    };
    let outcome = engine.advance_loop(&SweepContext {
        range_mode: true,
        frame_has_cuts: &has_cuts,
    });
    assert_eq!(outcome, StepOutcome::Stopped);
    assert!(
        !engine.cursor.playing,
        "Stopped clears playing (legacy arm)"
    );
    assert_eq!(engine.cursor.index, 0, "a stop never moves the cursor");
}

/// CLASS (b) — spec-decided normalize, LANDED at stage (iii). The legacy
/// plain-advance arm computed `(index + 1) % len` eagerly, so stepping an
/// EMPTY strip panicked with a divide-by-zero (latent, production-
/// unreachable — `maybe_advance_history_loop` gates on `history_playing`).
/// The unified stepper routes through `LoopEngine::advance_loop`, whose
/// empty-strip cell is a graceful `Stopped` that clears `playing`; this
/// test re-pins the landed behavior (it was `#[should_panic]` while the
/// legacy body owned the path).
#[test]
fn advance_diff_empty_history_now_stops_gracefully_normalize_landed() {
    let ctx = egui::Context::default();
    let mut app = test_viewer_app_with_hazards(Vec::new());
    app.primary.cursor.playing = true;
    app.advance_primary_screen_loop(&ctx);
    assert!(
        !app.primary.cursor.playing,
        "an empty strip stops the loop instead of panicking (class-(b) landed)"
    );
    assert_eq!(app.primary.cursor.index, 0);
}

/// Edge cells: an empty history STOPS the engine (the graceful side of the
/// legacy latent panic pinned above); a single-frame plain loop steps onto
/// itself and then stops via can-step (legacy) / mirror can-step (engine
/// caller) — differentially.
#[test]
fn advance_diff_empty_and_single_frame_edges_match() {
    let ctx = egui::Context::default();
    let product = ref_product();

    // Empty history: engine side (the legacy side panics — see the
    // companion `#[should_panic]` pin above).
    let mut engine = LoopEngine::new(
        EngineId(43),
        EngineRole::Primary,
        FeedSource::Live(SiteRef::Us {
            level2_id: "KEAX".to_owned(),
        }),
    );
    engine.cursor.playing = true;
    let mirror = drive_engine_advance(&mut engine, &product, None, 4);
    assert_eq!(
        mirror,
        vec![(0, 0, false)],
        "engine stops gracefully on an empty strip"
    );
    assert!(!engine.cursor.playing);

    // Single frame, machinery off: the step lands back on frame 0 and
    // playing clears because a one-frame plain loop cannot keep stepping.
    let fixture = ord_ref_only_fixture();
    let mut app = test_viewer_app_with_hazards(Vec::new());
    app.primary.history.push(fixture.frames[0].clone());
    app.primary.cursor.playing = true;
    app.volume = Some(Arc::clone(&fixture.frames[0].volume));
    let legacy = drive_legacy_advance(&mut app, &ctx, 4);
    let mut engine = LoopEngine::new(
        EngineId(44),
        EngineRole::Primary,
        FeedSource::Live(SiteRef::Us {
            level2_id: "deess".to_owned(),
        }),
    );
    engine.history = FrameHistory::from(vec![fixture.frames[0].clone()]);
    engine.cursor.playing = true;
    let mirror = drive_engine_advance(&mut engine, &product, None, 4);
    assert_eq!(legacy, mirror, "single-frame plain loop");
    assert_eq!(legacy, vec![(0, 0, false)]);
}

// ---------------------------------------------------------------------------
// Install differential — legacy P1/P3 bodies vs engine install_batch/
// install_frame, with the stage-(ii) caller's display model.
// ---------------------------------------------------------------------------

/// The engine plus the caller-owned state the stage-(ii) unify keeps in
/// ViewerApp: the displayed volume (written on `SelectedAnchor` and on the
/// census-D9 `display_install` outcome) — the engine never owns a display.
struct EngineMirror {
    engine: LoopEngine,
    display: Option<Arc<RadarVolume>>,
}

impl EngineMirror {
    fn primary(site: &str) -> Self {
        Self {
            engine: LoopEngine::new(
                EngineId(45),
                EngineRole::Primary,
                FeedSource::Live(SiteRef::Us {
                    level2_id: site.to_owned(),
                }),
            ),
            display: None,
        }
    }

    fn install_batch(
        &mut self,
        batch: DecodedLoadBatch,
        policy: &SelectionPolicy,
        defer_anchor: impl FnOnce(&FrameHistoryEntry) -> bool,
    ) -> InstallOutcome {
        let active = self.display.clone();
        let outcome = self
            .engine
            .install_batch(batch, policy, active.as_ref(), defer_anchor);
        if let InstallSelection::SelectedAnchor { index } = outcome.selection {
            self.display = self
                .engine
                .history
                .get(index)
                .map(|frame| Arc::clone(&frame.volume));
        }
        if let Some(index) = outcome.display_install {
            // Census D9, owner decision LIVE WINS: the caller writes the
            // display even while the cursor holds.
            self.display = self
                .engine
                .history
                .get(index)
                .map(|frame| Arc::clone(&frame.volume));
        }
        outcome
    }

    /// The stage-(ii) shim body for the polled route: the legacy
    /// `install_polled_volume` frame shape (path `poll://{name}`, status
    /// Complete, label `polled {name}`) through `install_frame` +
    /// `FollowNewestUnlessPlaying` (census D5a); single-frame routes never
    /// defer (census D7 row P3).
    fn install_polled(&mut self, name: &str, volume: Arc<RadarVolume>) -> InstallOutcome {
        let decoded = DecodedLoad {
            path: PathBuf::from(format!("poll://{name}")),
            volume,
            timings: LoadTimings::default(),
            status: FrameStatus::Complete,
            source_label: format!("polled {name}"),
        };
        self.install_batch(
            DecodedLoadBatch::single(decoded),
            &SelectionPolicy::FollowNewestUnlessPlaying,
            |_| false,
        )
    }
}

/// One comparable history row. `timings` is deliberately EXCLUDED — the
/// polled-route timings field difference is the class-(c) cell pinned red
/// below, and census D6 rules timings "perf-telemetry plumbing, not policy".
type FrameRow = (String, DateTime<Utc>, PathBuf, FrameStatus, usize);

/// The observable loop state both sides must agree on.
#[derive(Debug, PartialEq)]
struct LoopState {
    frames: Vec<FrameRow>,
    cursor: usize,
    playing: bool,
    browsing: bool,
    frame_limit: usize,
    display_ptr: Option<usize>,
}

fn frame_rows(history: &FrameHistory) -> Vec<FrameRow> {
    history
        .iter()
        .map(|frame| {
            (
                frame.identity.site_id.clone(),
                frame.identity.scan_time_utc,
                frame.path.clone(),
                frame.status,
                Arc::as_ptr(&frame.volume) as usize,
            )
        })
        .collect()
}

fn legacy_state(app: &ViewerApp) -> LoopState {
    LoopState {
        frames: frame_rows(&app.primary.history),
        cursor: app.primary.cursor.index,
        playing: app.primary.cursor.playing,
        browsing: app.primary.cursor.browsing,
        frame_limit: app.primary.limits.frame_limit,
        display_ptr: app
            .volume
            .as_ref()
            .map(|volume| Arc::as_ptr(volume) as usize),
    }
}

fn mirror_state(mirror: &EngineMirror) -> LoopState {
    LoopState {
        frames: frame_rows(&mirror.engine.history),
        cursor: mirror.engine.cursor.index,
        playing: mirror.engine.cursor.playing,
        browsing: mirror.engine.cursor.browsing,
        frame_limit: mirror.engine.limits.frame_limit,
        display_ptr: mirror
            .display
            .as_ref()
            .map(|volume| Arc::as_ptr(volume) as usize),
    }
}

fn install_time(minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 9, 5, minute, 0).unwrap()
}

/// A bare decoded load (no cuts — install tests exercise ordering and
/// guards, not sweep structure). Built ONCE per scenario and cloned to each
/// side so volume-Arc identity is comparable across sides.
fn bare_load(site: &str, minute: u32, path: &str, status: FrameStatus) -> DecodedLoad {
    DecodedLoad {
        path: PathBuf::from(path),
        volume: Arc::new(RadarVolume::new(
            radar_core::RadarSite::new(site),
            install_time(minute),
        )),
        timings: LoadTimings::default(),
        status,
        source_label: format!("realtime L2 {site}"),
    }
}

fn complete_load(site: &str, minute: u32, path: &str) -> DecodedLoad {
    bare_load(site, minute, path, FrameStatus::Complete)
}

fn batch_anchored_last(frames: Vec<DecodedLoad>) -> DecodedLoadBatch {
    let selected_index = frames.len().saturating_sub(1);
    DecodedLoadBatch {
        frames,
        selected_index,
    }
}

/// Census D1/D2/D5/D16: a fresh out-of-order batch installs sorted and
/// selects the anchor on both sides; a re-delivered overlap upserts without
/// duplicating; the legacy bool equals `InstallOutcome::anchor_selected`.
#[test]
fn install_diff_p1_batch_select_and_upsert_match_engine_select_anchor() {
    let ctx = egui::Context::default();
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("KEAX");

    let first_batch = DecodedLoadBatch {
        frames: vec![
            complete_load("KEAX", 30, "keax-c"),
            complete_load("KEAX", 10, "keax-a"),
            complete_load("KEAX", 20, "keax-b"),
        ],
        selected_index: 0, // anchor = the 05:30 frame, delivered first
    };
    let legacy_selected = app.install_decoded_load_batch(first_batch.clone(), false, true, &ctx);
    let outcome = mirror.install_batch(first_batch, &SELECT_PRIMARY, |_| false);
    assert_eq!(legacy_selected, outcome.anchor_selected(), "D16 bool");
    assert!(outcome.anchor_selected());
    assert_eq!(legacy_state(&app), mirror_state(&mirror), "fresh batch");
    assert_eq!(app.primary.cursor.index, 2, "anchor after ascending sort");

    // Overlap: 05:10 re-delivered on the same path (refresh-only, rule b);
    // 05:20 from a NEW path (rule c replaces); 05:40 new.
    let overlap = DecodedLoadBatch {
        frames: vec![
            complete_load("KEAX", 10, "keax-a"),
            complete_load("KEAX", 20, "keax-b-archive"),
            complete_load("KEAX", 40, "keax-d"),
        ],
        selected_index: 2,
    };
    let legacy_selected = app.install_decoded_load_batch(overlap.clone(), false, true, &ctx);
    let outcome = mirror.install_batch(overlap, &SELECT_PRIMARY, |_| false);
    assert_eq!(
        legacy_selected,
        outcome.anchor_selected(),
        "D16 bool (overlap)"
    );
    assert_eq!(legacy_state(&app), mirror_state(&mirror), "overlap upsert");
    assert_eq!(app.primary.history.len(), 4, "upsert never duplicates");
}

/// Census D5: playing and browsing block the anchor select and fall into the
/// engine-common backfill reposition (D5c) with the legacy "Backfilled"
/// status; the D16 bool is false on both sides.
#[test]
fn install_diff_p1_playing_and_browsing_guards_backfill_match_engine() {
    let ctx = egui::Context::default();
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("KEAX");

    let seed = DecodedLoadBatch::single(complete_load("KEAX", 10, "keax-a"));
    let _ = app.install_decoded_load_batch(seed.clone(), false, true, &ctx);
    let _ = mirror.install_batch(seed, &SELECT_PRIMARY, |_| false);

    // Playing guard.
    app.primary.cursor.playing = true;
    mirror.engine.cursor.playing = true;
    let growth = batch_anchored_last(vec![
        complete_load("KEAX", 5, "keax-older"),
        complete_load("KEAX", 40, "keax-newer"),
    ]);
    let legacy_selected = app.install_decoded_load_batch(growth.clone(), false, true, &ctx);
    let outcome = mirror.install_batch(growth, &SELECT_PRIMARY, |_| false);
    assert!(!legacy_selected, "playing blocks the select");
    assert_eq!(legacy_selected, outcome.anchor_selected(), "D16 bool");
    assert_eq!(
        outcome.selection,
        InstallSelection::Backfilled { index: 1 },
        "displayed frame reposition after indices shifted"
    );
    assert_eq!(app.status, "Backfilled KEAX", "legacy backfill status");
    assert_eq!(legacy_state(&app), mirror_state(&mirror), "playing guard");

    // Browsing guard (not playing).
    app.primary.cursor.playing = false;
    mirror.engine.cursor.playing = false;
    app.primary.cursor.browsing = true;
    mirror.engine.cursor.browsing = true;
    let newest = DecodedLoadBatch::single(complete_load("KEAX", 50, "keax-newest"));
    let legacy_selected = app.install_decoded_load_batch(newest.clone(), false, true, &ctx);
    let outcome = mirror.install_batch(newest, &SELECT_PRIMARY, |_| false);
    assert!(!legacy_selected, "browsing blocks the select");
    assert_eq!(legacy_selected, outcome.anchor_selected(), "D16 bool");
    assert_eq!(legacy_state(&app), mirror_state(&mirror), "browsing guard");
}

/// Census D5b: the Primary-only blank-display escape — browsing with nothing
/// displayed still selects the anchor and force-exits browse mode.
#[test]
fn install_diff_p1_blank_display_browsing_escape_matches_engine() {
    let ctx = egui::Context::default();
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("KEAX");
    app.primary.cursor.browsing = true;
    mirror.engine.cursor.browsing = true;

    let batch = DecodedLoadBatch::single(complete_load("KEAX", 10, "keax-a"));
    let legacy_selected = app.install_decoded_load_batch(batch.clone(), false, true, &ctx);
    let outcome = mirror.install_batch(batch, &SELECT_PRIMARY, |_| false);
    assert!(legacy_selected, "blank display overrides browsing");
    assert_eq!(legacy_selected, outcome.anchor_selected(), "D16 bool");
    assert!(!app.primary.cursor.browsing, "legacy exits browse mode");
    assert_eq!(legacy_state(&app), mirror_state(&mirror), "blank escape");
}

/// Census D3/D4: the cross-site guard clears BEFORE install with the
/// greppable-identical diagnostic. The non-select (Backfill) variant leaves
/// the legacy status holding the diagnostic, so the wording is asserted
/// against `CrossSiteClear::diagnostic` character for character.
#[test]
fn install_diff_p1_cross_site_clear_diagnostic_and_state_match_engine() {
    let ctx = egui::Context::default();
    let seed = batch_anchored_last(vec![
        complete_load("KEAX", 10, "keax-a"),
        complete_load("KEAX", 20, "keax-b"),
        complete_load("KEAX", 30, "keax-c"),
    ]);
    let foreign = DecodedLoadBatch::single(complete_load("KTLX", 40, "ktlx-a"));

    // select=false → the legacy Backfill route; the displayed KEAX frame is
    // gone after the clear, so the diagnostic stays in `status`.
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("KEAX");
    let _ = app.install_decoded_load_batch(seed.clone(), false, true, &ctx);
    let _ = mirror.install_batch(seed.clone(), &SELECT_PRIMARY, |_| false);
    let legacy_selected = app.install_decoded_load_batch(foreign.clone(), false, false, &ctx);
    let outcome = mirror.install_batch(foreign.clone(), &SelectionPolicy::Backfill, |_| false);
    assert!(!legacy_selected);
    assert!(!outcome.anchor_selected(), "D16 bool");
    let clear = outcome.cross_site_clear.expect("guard must fire");
    assert_eq!(
        app.status, clear.diagnostic,
        "the legacy diagnostic wording IS the engine's CrossSiteClear::diagnostic"
    );
    assert_eq!(
        clear.diagnostic,
        "history reset (site change to KTLX, had 3 frames)"
    );
    assert_eq!(clear.dropped_frames, 3);
    assert_eq!(
        legacy_state(&app),
        mirror_state(&mirror),
        "cross-site clear"
    );

    // select=true variant while playing: the clear stops playback and the
    // anchor then selects on both sides (pinned legacy behavior).
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("KEAX");
    let _ = app.install_decoded_load_batch(seed.clone(), false, true, &ctx);
    let _ = mirror.install_batch(seed, &SELECT_PRIMARY, |_| false);
    app.primary.cursor.playing = true;
    mirror.engine.cursor.playing = true;
    let legacy_selected = app.install_decoded_load_batch(foreign.clone(), false, true, &ctx);
    let outcome = mirror.install_batch(foreign, &SELECT_PRIMARY, |_| false);
    assert!(
        legacy_selected,
        "the clear stops playback, then the select runs"
    );
    assert_eq!(legacy_selected, outcome.anchor_selected(), "D16 bool");
    assert!(outcome.cross_site_clear.is_some());
    assert_eq!(
        legacy_state(&app),
        mirror_state(&mirror),
        "cross-site select"
    );
}

/// Census D2: the 3-tier upsert ladder — a live-partial refetch with more
/// radials replaces (rule a); a lower-priority Preview never replaces; a
/// Complete from a new path replaces (rule c) — identical on both sides.
#[test]
fn install_diff_p1_live_partial_upsert_ladder_matches_engine() {
    let ctx = egui::Context::default();
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("KEAX");

    let partial = |radials: usize| {
        let mut load = bare_load("KEAX", 10, "keax-chunk", FrameStatus::LivePartial);
        let mut volume = (*load.volume).clone();
        volume.metadata.decoded_radial_count = radials;
        load.volume = Arc::new(volume);
        load
    };
    let step = |app: &mut ViewerApp, mirror: &mut EngineMirror, load: DecodedLoad, label: &str| {
        let batch = DecodedLoadBatch::single(load);
        let legacy_selected = app.install_decoded_load_batch(batch.clone(), false, true, &ctx);
        let outcome = mirror.install_batch(batch, &SELECT_PRIMARY, |_| false);
        assert_eq!(
            legacy_selected,
            outcome.anchor_selected(),
            "D16 bool: {label}"
        );
        assert_eq!(legacy_state(app), mirror_state(mirror), "{label}");
    };

    step(&mut app, &mut mirror, partial(100), "fresh live-partial");
    step(
        &mut app,
        &mut mirror,
        partial(200),
        "rule (a): more radials replace",
    );
    step(
        &mut app,
        &mut mirror,
        bare_load("KEAX", 10, "keax-preview", FrameStatus::Preview),
        "rule (c) downgrade arm: Preview never replaces",
    );
    assert_eq!(app.primary.history[0].status, FrameStatus::LivePartial);
    step(
        &mut app,
        &mut mirror,
        complete_load("KEAX", 10, "keax-archive"),
        "rule (c) upgrade arm: Complete from a new path replaces",
    );
    assert_eq!(app.primary.history[0].status, FrameStatus::Complete);
}

/// Census D7: a live-partial anchor that cannot materialize the active
/// product defers — the batch stays installed, selection is withheld, the
/// cursor reparks on the active frame, and the caller-composed status
/// matches the legacy wording. The defer predicate is the SAME legacy
/// helper on both sides (caller-supplied, per the D7 decision).
#[test]
fn install_diff_p1_live_partial_deferral_matches_engine_defer_hook() {
    let ctx = egui::Context::default();
    let velocity = DisplayProduct::Moment(MomentType::Velocity);
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("KEAX");
    app.selected_product = velocity.clone();

    // Active frame: a complete volume that CAN materialize velocity.
    let mut active_volume = test_velocity_sails_volume_with_radials(&[(0.5, 0)], 720);
    active_volume.site = radar_core::RadarSite::new("KEAX");
    active_volume.volume_time = install_time(10);
    let seed = DecodedLoadBatch::single(DecodedLoad {
        path: PathBuf::from("keax-velocity"),
        volume: Arc::new(active_volume),
        timings: LoadTimings::default(),
        status: FrameStatus::Complete,
        source_label: "realtime L2 KEAX".to_owned(),
    });
    let _ = app.install_decoded_load_batch(seed.clone(), false, true, &ctx);
    let _ = mirror.install_batch(seed, &SELECT_PRIMARY, |_| false);
    assert_eq!(legacy_state(&app), mirror_state(&mirror), "velocity seed");

    // Candidate: a REF-only live partial — velocity cannot materialize.
    let mut candidate_volume = fixture_volume("KEAX", install_time(20), &[(0.5, 0, 720)]);
    candidate_volume.metadata.decoded_radial_count = 720;
    let batch = DecodedLoadBatch::single(DecodedLoad {
        path: PathBuf::from("keax-ref-partial"),
        volume: Arc::new(candidate_volume),
        timings: LoadTimings::default(),
        status: FrameStatus::LivePartial,
        source_label: "realtime L2 KEAX".to_owned(),
    });
    let legacy_selected = app.install_decoded_load_batch(batch.clone(), false, true, &ctx);
    assert!(!legacy_selected, "legacy defers");
    assert_eq!(
        app.status,
        format!("Waiting for {} in KEAX", velocity.label()),
        "legacy defer status (product label is app-side wording)"
    );

    let active_arc = mirror.display.clone().expect("seed installed a display");
    let selected_cut = app.selected_cut;
    let outcome = mirror.install_batch(batch, &SELECT_PRIMARY, |anchor| {
        should_defer_live_partial_selection_for_active_product(
            Some(active_arc.as_ref()),
            selected_cut,
            &velocity,
            Some(anchor),
            false, // no manual cut hold on either side
        )
    });
    assert!(!outcome.anchor_selected(), "D16 bool");
    match &outcome.selection {
        InstallSelection::DeferredLivePartial { anchor } => {
            assert_eq!(
                app.status,
                format!("Waiting for {} in {}", velocity.label(), anchor.site_id),
                "the caller composes the legacy wording from the outcome"
            );
        }
        other => panic!("expected deferral, got {other:?}"),
    }
    assert_eq!(legacy_state(&app), mirror_state(&mirror), "deferral");
    assert_eq!(
        app.primary.history.len(),
        2,
        "deferral leaves the batch installed"
    );
}

/// Census D8: the legacy grow-limit triggers (ORD `ord-archive:` path
/// sniffing and the one-shot local-autoplay flag) map onto the engine's
/// `grow_to_fit` INTENT; without the intent the same delivery trims.
#[test]
fn install_diff_p1_grow_limit_triggers_match_engine_grow_to_fit_intent() {
    let ctx = egui::Context::default();

    // ORD archive loop: legacy sniffs the path prefix; the adopted caller
    // states intent instead (the census kills the sniffing heuristic).
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("KEAX");
    mirror.engine.limits.grow_to_fit = true;
    let ord_batch = batch_anchored_last(
        (0..10)
            .map(|index| complete_load("KEAX", index, &format!("ord-archive:frame-{index}")))
            .collect(),
    );
    let legacy_selected = app.install_decoded_load_batch(ord_batch.clone(), false, true, &ctx);
    let outcome = mirror.install_batch(ord_batch, &SELECT_PRIMARY, |_| false);
    assert_eq!(legacy_selected, outcome.anchor_selected(), "D16 bool");
    assert_eq!(app.primary.limits.frame_limit, 10, "legacy grew the limit");
    assert_eq!(legacy_state(&app), mirror_state(&mirror), "ord grow");

    // Local deployment autoplay: the one-shot pending flag is the intent.
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("KEAX");
    app.pending_local_autoplay = true;
    mirror.engine.limits.grow_to_fit = true;
    let local_batch = batch_anchored_last(
        (0..9)
            .map(|index| bare_load("KEAX", index, &format!("local:{index}"), FrameStatus::Local))
            .collect(),
    );
    let _ = app.install_decoded_load_batch(local_batch.clone(), false, true, &ctx);
    let _ = mirror.install_batch(local_batch, &SELECT_PRIMARY, |_| false);
    assert!(!app.pending_local_autoplay, "the legacy flag is one-shot");
    assert_eq!(app.primary.limits.frame_limit, 9);
    assert_eq!(legacy_state(&app), mirror_state(&mirror), "local grow");

    // No intent: the same oversized delivery trims to the limit on both
    // sides (and the trim writes the normalized limit back — census D8).
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("KEAX");
    let plain_batch = batch_anchored_last(
        (0..10)
            .map(|index| complete_load("KEAX", index, &format!("plain:{index}")))
            .collect(),
    );
    let _ = app.install_decoded_load_batch(plain_batch.clone(), false, true, &ctx);
    let _ = mirror.install_batch(plain_batch, &SELECT_PRIMARY, |_| false);
    assert_eq!(app.primary.history.len(), app.primary.limits.frame_limit);
    assert_eq!(legacy_state(&app), mirror_state(&mirror), "no-intent trim");
}

/// Census D1 (order), fixture tier: each differential fixture delivered as
/// one shuffled batch lands ascending by identity, identically on both
/// sides, with the anchor found through the post-sort identity mapping.
#[test]
fn install_diff_fixture_batches_order_identically() {
    let ctx = egui::Context::default();
    for fixture in [
        keax_derecho_fixture(),
        jma_merged_fixture(),
        ord_ref_only_fixture(),
    ] {
        // Deterministic shuffle: odd indices first, then evens reversed.
        let mut shuffled: Vec<&FrameHistoryEntry> = Vec::new();
        shuffled.extend(fixture.frames.iter().skip(1).step_by(2));
        shuffled.extend(fixture.frames.iter().step_by(2).rev());
        let newest = &fixture.frames[fixture.frames.len() - 1];
        let anchor_position = shuffled
            .iter()
            .position(|frame| frame.identity == newest.identity)
            .expect("anchor present");
        let batch = DecodedLoadBatch {
            frames: shuffled
                .iter()
                .map(|frame| DecodedLoad {
                    path: frame.path.clone(),
                    volume: Arc::clone(&frame.volume),
                    timings: LoadTimings::default(),
                    status: frame.status,
                    source_label: frame.source_label.clone(),
                })
                .collect(),
            selected_index: anchor_position,
        };

        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.primary.limits.frame_limit = 96;
        let mut mirror = EngineMirror::primary(fixture.site);
        mirror.engine.limits.frame_limit = 96;

        let legacy_selected = app.install_decoded_load_batch(batch.clone(), false, true, &ctx);
        let outcome = mirror.install_batch(batch, &SELECT_PRIMARY, |_| false);
        assert_eq!(
            legacy_selected,
            outcome.anchor_selected(),
            "D16 bool: {}",
            fixture.name
        );
        assert_eq!(
            legacy_state(&app),
            mirror_state(&mirror),
            "shuffled batch: {}",
            fixture.name
        );
        let times: Vec<DateTime<Utc>> = app
            .primary
            .history
            .iter()
            .map(|frame| frame.identity.scan_time_utc)
            .collect();
        let mut sorted = times.clone();
        sorted.sort();
        assert_eq!(times, sorted, "ascending by identity: {}", fixture.name);
        assert_eq!(
            app.primary.cursor.index,
            app.primary.history.len() - 1,
            "anchor selected through the identity mapping: {}",
            fixture.name
        );
    }
}

/// Census D2/D3/D5a/D9/D10: the polled route, tick by tick — follow, rule-c
/// replace under a new `poll://` path, cursor hold while playing WITH the
/// LIVE-WINS display install, browsing deliberately not blocking the
/// follow, cross-site clear (silent in legacy P3; the engine GAINS the P1
/// diagnostic per the D3 decision — a class-(b) normalize), and trim.
#[test]
fn install_diff_p3_polled_tick_sequence_matches_engine_follow_newest() {
    let ctx = egui::Context::default();
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("FWLX");

    let volume = |site: &str, minute: u32| {
        Arc::new(RadarVolume::new(
            radar_core::RadarSite::new(site),
            install_time(minute),
        ))
    };
    let tick = |app: &mut ViewerApp,
                mirror: &mut EngineMirror,
                name: &str,
                volume: Arc<RadarVolume>,
                label: &str|
     -> InstallOutcome {
        app.install_polled_volume(name, Arc::clone(&volume), &ctx);
        let outcome = mirror.install_polled(name, volume);
        assert_eq!(legacy_state(app), mirror_state(mirror), "{label}");
        outcome
    };

    let outcome = tick(
        &mut app,
        &mut mirror,
        "FWLX_0510",
        volume("FWLX", 10),
        "first tick",
    );
    assert_eq!(
        outcome.selection,
        InstallSelection::FollowedNewest { index: 0 }
    );
    let _ = tick(
        &mut app,
        &mut mirror,
        "FWLX_0520",
        volume("FWLX", 20),
        "follow newer",
    );
    assert_eq!(app.primary.cursor.index, 1);

    // Repeat identity under a NEW poll name → new path → rule (c) replaces
    // (the census-D2 fixture cell: every current polled caller keeps
    // winning through rule (c)).
    let repeat = volume("FWLX", 20);
    let repeat_ptr = Arc::as_ptr(&repeat) as usize;
    let _ = tick(
        &mut app,
        &mut mirror,
        "FWLX_0520b",
        repeat,
        "rule (c) repeat",
    );
    assert_eq!(app.primary.history.len(), 2, "identity match replaces");
    assert_eq!(
        Arc::as_ptr(&app.primary.history[1].volume) as usize,
        repeat_ptr,
        "the NEW volume Arc is stored"
    );

    // Playing loop: the cursor holds, the display still takes the newest
    // volume (census D9, owner decision LIVE WINS).
    app.primary.cursor.playing = true;
    app.primary.cursor.index = 0;
    mirror.engine.cursor.playing = true;
    mirror.engine.cursor.index = 0;
    let newest = volume("FWLX", 30);
    let newest_ptr = Arc::as_ptr(&newest) as usize;
    let outcome = tick(&mut app, &mut mirror, "FWLX_0530", newest, "playing hold");
    assert_eq!(app.primary.cursor.index, 0, "playing loop keeps its cursor");
    assert_eq!(
        outcome.selection,
        InstallSelection::HeldWhilePlaying { newest_index: 2 }
    );
    assert_eq!(
        outcome.display_install,
        Some(2),
        "LIVE WINS display install"
    );
    assert_eq!(
        app.volume
            .as_ref()
            .map(|volume| Arc::as_ptr(volume) as usize),
        Some(newest_ptr),
        "legacy display shows the newest volume while the cursor holds"
    );

    // Browsing does NOT block the follow (census D5a, deliberate).
    app.primary.cursor.playing = false;
    app.primary.cursor.browsing = true;
    mirror.engine.cursor.playing = false;
    mirror.engine.cursor.browsing = true;
    let _ = tick(
        &mut app,
        &mut mirror,
        "FWLX_0540",
        volume("FWLX", 40),
        "browsing follow",
    );
    assert_eq!(
        app.primary.cursor.index, 3,
        "browsing is deliberately ignored"
    );
    assert!(app.primary.cursor.browsing, "the flag itself survives");

    // Cross-site: legacy P3 clears SILENTLY (census D3 row P3); the engine
    // outcome carries the P1 diagnostic — the census D3 DECISION (class
    // (b) normalize: the polled path GAINS the diagnostic at adoption,
    // wording greppable-identical to P1's).
    let outcome = tick(
        &mut app,
        &mut mirror,
        "KTLX_0550",
        volume("KTLX", 50),
        "cross-site",
    );
    assert!(
        !app.status.contains("history reset"),
        "legacy P3 wrote no clear diagnostic — the D3 normalize adds it at adoption"
    );
    let clear = outcome.cross_site_clear.expect("guard fires");
    assert_eq!(
        clear.diagnostic, "history reset (site change to KTLX, had 4 frames)",
        "the engine diagnostic uses the P1 wording (census D3 decision)"
    );
}

/// Census D8 trim interplay on the polled route: a small frame limit trims
/// oldest-first on every tick, the limit write-back matches, and the
/// follow lands on the (shifted) newest index — identically.
#[test]
fn install_diff_p3_trim_and_cursor_clamp_match_engine() {
    let ctx = egui::Context::default();
    let mut app = test_viewer_app_with_hazards(Vec::new());
    app.primary.limits.frame_limit = 3;
    let mut mirror = EngineMirror::primary("FWLX");
    mirror.engine.limits.frame_limit = 3;

    for minute in [10u32, 15, 20, 25, 30] {
        let volume = Arc::new(RadarVolume::new(
            radar_core::RadarSite::new("FWLX"),
            install_time(minute),
        ));
        app.install_polled_volume(&format!("FWLX_{minute}"), Arc::clone(&volume), &ctx);
        let _ = mirror.install_polled(&format!("FWLX_{minute}"), volume);
        assert_eq!(legacy_state(&app), mirror_state(&mirror), "tick {minute}");
    }
    assert_eq!(app.primary.history.len(), 3, "trimmed to the limit");
    assert_eq!(app.primary.cursor.index, 2, "follow lands on the newest");
    assert_eq!(app.primary.limits.frame_limit, 3, "write-back normalized");
}

/// CLASS (c) — RED CELL, awaiting an owner decision at this gate.
///
/// A growing SAME-NAME file re-poll (dir.list signature grew, name did
/// not — live behavior pinned by
/// `dir_list_signatures_keep_growing_same_name_pollable`) re-delivers the
/// same identity under the SAME `poll://` path. Legacy P3 replaces the
/// stored volume Arc unconditionally (history AND display show the grown
/// file); the engine's census-D2 rule (b) refreshes metadata only and
/// KEEPS the old Arc — and because the D9 `display_install` outcome
/// indexes into that history, the DISPLAY holds the stale volume too: the
/// user would stop seeing a growing live file update at all. The census D2
/// decision required "rule (c) must keep every current P3 caller winning"
/// and justified it with "the poll:// path always differs per tick" —
/// FALSE for this cell. The 4c
/// engine deliberately pinned its side
/// (`same_path_same_status_repoll_refreshes_metadata_only_the_4e_growing_file_gap`)
/// and deferred the resolution to THIS gate. Un-ignore once the owner
/// decides (either extend the engine upsert so a same-path Complete
/// re-poll with new data replaces, or accept the metadata-only refresh and
/// re-pin the legacy test).
#[test]
#[ignore = "class (c) 4e gate finding: growing same-name re-poll — legacy replaces the \
            volume Arc, engine rule (b) keeps it; census D2's rationale does not cover \
            this cell; owner decision required before the P3 shim lands"]
fn install_diff_p3_growing_same_name_repoll_must_match_legacy_replace() {
    let ctx = egui::Context::default();
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("FWLX");

    let small = Arc::new(RadarVolume::new(
        radar_core::RadarSite::new("FWLX"),
        install_time(10),
    ));
    app.install_polled_volume("FWLX_0510", Arc::clone(&small), &ctx);
    let _ = mirror.install_polled("FWLX_0510", small);

    // The same file, re-polled after it grew: same name → same poll://
    // path, same identity, MORE decoded radials.
    let mut grown_volume = RadarVolume::new(radar_core::RadarSite::new("FWLX"), install_time(10));
    grown_volume.metadata.decoded_radial_count = 720;
    let grown = Arc::new(grown_volume);
    app.install_polled_volume("FWLX_0510", Arc::clone(&grown), &ctx);
    let _ = mirror.install_polled("FWLX_0510", grown);

    assert_eq!(
        legacy_state(&app),
        mirror_state(&mirror),
        "RED: legacy history holds the grown volume Arc; engine rule (b) kept the old one"
    );
}

/// CLASS (c) — RED CELL, awaiting an owner decision at this gate (minor).
///
/// Legacy P3 builds its history entry with `timings: None`; the engine
/// install path derives entries from `DecodedLoad` and always stores
/// `Some(timings)` — the polled shim would store
/// `Some(LoadTimings::default())`, so frame-timing readouts change from
/// absent to zeroed. Census D6 calls timings "perf-telemetry plumbing, not
/// policy", which is why the main state comparison above excludes the
/// field — but the port must not change it silently. Un-ignore once the
/// owner decides (the shim keeps None via a post-install fix-up,
/// `DecodedLoad.timings` becomes Option, or the zeroed default is accepted
/// and this cell re-pins that).
#[test]
#[ignore = "class (c) 4e gate finding: polled entries carry timings None in legacy but \
            Some(default) through the engine install; decide before the P3 shim lands"]
fn install_diff_p3_polled_entry_timings_field_must_match_legacy() {
    let ctx = egui::Context::default();
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let mut mirror = EngineMirror::primary("FWLX");
    let volume = Arc::new(RadarVolume::new(
        radar_core::RadarSite::new("FWLX"),
        install_time(10),
    ));
    app.install_polled_volume("FWLX_0510", Arc::clone(&volume), &ctx);
    let _ = mirror.install_polled("FWLX_0510", volume);

    assert_eq!(
        app.primary.history[0].timings.is_none(),
        mirror.engine.history[0].timings.is_none(),
        "RED: legacy polled entries have timings None, engine entries Some(default)"
    );
}

// ---------------------------------------------------------------------------
// Liveness differential — the mode_chip_state family vs chips derived from
// LoopEngine::liveness(), the R8 truth table.
// ---------------------------------------------------------------------------

/// The adoption-shape chip renderer: `engine.liveness()` → the exact legacy
/// chip label/tag. Stage (iii) installed exactly this shape as
/// `ViewerApp::mode_chip_state` (over `liveness_with_live_flag` +
/// `chip_for_liveness`); this INDEPENDENT copy keeps the truth-table tests
/// a real differential — a drift in the app renderer fails the cells here.
fn chip_from_engine(
    engine: &LoopEngine,
    user_stale_chip_seconds: i64,
) -> Option<(String, &'static str)> {
    let newest_time = engine.history.last()?.identity.scan_time_utc;
    let chip = match engine.liveness(Utc::now(), user_stale_chip_seconds)? {
        Liveness::Live { stale: false, .. } => ("⏺ LIVE".to_owned(), "LIVE"),
        Liveness::Live {
            age_seconds,
            stale: true,
        } => (format!("⏺ LIVE · STALE {}m", age_seconds / 60), "STALE"),
        Liveness::Archive { age_seconds } => {
            if age_seconds >= 24 * 60 * 60 {
                (
                    format!("ARCHIVE · {}", newest_time.format("%Y-%m-%d %H:%MZ")),
                    "ARCH",
                )
            } else {
                (format!("ARCHIVE · {}m old", age_seconds / 60), "ARCH")
            }
        }
    };
    Some(chip)
}

fn aged_volume(site: &str, age_seconds: i64) -> Arc<RadarVolume> {
    Arc::new(RadarVolume::new(
        radar_core::RadarSite::new(site),
        Utc::now() - ChronoDuration::seconds(age_seconds),
    ))
}

fn engine_with_volume(role: EngineRole, feed: FeedSource, volume: &Arc<RadarVolume>) -> LoopEngine {
    let mut engine = LoopEngine::new(EngineId(46), role, feed);
    engine.history.push(FrameHistoryEntry {
        identity: frame_identity_for_volume(volume),
        path: PathBuf::from("liveness-cell"),
        volume: Arc::clone(volume),
        timings: None,
        status: FrameStatus::Complete,
        source_label: "liveness cell".to_owned(),
    });
    engine
}

/// Seed the app's display AND the matching single history frame. Since the
/// stage-(iii) unify the primary chip derives from the NEWEST history frame
/// (spec §3), so the app side of every chip cell must carry its aged volume
/// on the frame strip, exactly like the engine side does.
fn seed_primary_chip_frame(app: &mut ViewerApp, volume: &Arc<RadarVolume>) {
    app.primary.history.push(FrameHistoryEntry {
        identity: frame_identity_for_volume(volume),
        path: PathBuf::from("liveness-cell"),
        volume: Arc::clone(volume),
        timings: None,
        status: FrameStatus::Complete,
        source_label: "liveness cell".to_owned(),
    });
    app.volume = Some(Arc::clone(volume));
}

/// Ages chosen mid-minute with ≥30 s margin from every boundary (user
/// threshold 480 s, intl floor 1800 s, the 24 h dated-archive switch), so
/// the two sides' independent `Utc::now()` reads can never straddle a
/// boundary.
const CHIP_AGES: [i64; 6] = [90, 630, 1230, 2430, 82_830, 90_030];

/// The R8 truth table, Primary role, US feed (the US/custom-URL primary
/// keeps `FeedSource::CustomUrl` until the polled path unifies): every
/// (live, age) cell of `mode_chip_state` — since stage (iii) itself a
/// derivation over `LoopEngine::liveness_with_live_flag` — must equal the
/// suite's independently-built engine chip, cell for cell.
#[test]
fn liveness_diff_primary_us_chip_truth_table_matches_engine_derivation() {
    for live in [true, false] {
        for age in CHIP_AGES {
            let mut app = test_viewer_app_with_hazards(Vec::new());
            app.primary.live.enabled = live;
            let volume = aged_volume("KEAX", age);
            seed_primary_chip_frame(&mut app, &volume);
            let legacy = app
                .mode_chip_state()
                .map(|(label, _color, tag)| (label, tag));

            let mut engine = engine_with_volume(
                EngineRole::Primary,
                FeedSource::CustomUrl(String::new()),
                &volume,
            );
            engine.live.enabled = live;
            let user = app.style_registry.radar_age().stale_chip_seconds;
            let derived = chip_from_engine(&engine, user);

            assert_eq!(legacy, derived, "US primary cell (live={live}, age={age}s)");
        }
    }
}

/// The R8 truth table, Primary role, international feed: the unified chip
/// bridges `intl_poll_owns_primary()` in as the live flag and derives the
/// CADENCE-AWARE floor inside the engine (spec §12 owner decision 3 —
/// `max(user, 1800, 2×60 s)` = 1800 at the intl catalog cadence, so the
/// labels are byte-identical to the old flat-floor chip); the suite's
/// engine side must reproduce every cell.
#[test]
fn liveness_diff_primary_intl_chip_floor_cells_match_engine() {
    let intl_feed = FeedSource::Live(SiteRef::Intl {
        provider_id: "ord".to_owned(),
        site_id: "deess".to_owned(),
    });
    for live in [true, false] {
        for age in CHIP_AGES {
            let mut app = test_viewer_app_with_hazards(Vec::new());
            app.primary.feed = intl_feed.clone();
            app.poll_active = live; // intl_poll_owns_primary() = poll_active && armed
            let volume = aged_volume("deess", age);
            seed_primary_chip_frame(&mut app, &volume);
            let legacy = app
                .mode_chip_state()
                .map(|(label, _color, tag)| (label, tag));

            let mut engine = engine_with_volume(EngineRole::Primary, intl_feed.clone(), &volume);
            engine.live.enabled = live;
            let user = app.style_registry.radar_age().stale_chip_seconds;
            let derived = chip_from_engine(&engine, user);

            assert_eq!(
                legacy, derived,
                "intl primary cell (live={live}, age={age}s)"
            );
        }
    }
}

/// The empty cells: no displayed volume (legacy) and no history frame
/// (engine) both yield no chip.
#[test]
fn liveness_diff_none_cells_match() {
    let app = test_viewer_app_with_hazards(Vec::new());
    assert_eq!(app.mode_chip_state(), None, "legacy: no volume, no chip");
    let engine = LoopEngine::new(
        EngineId(47),
        EngineRole::Primary,
        FeedSource::CustomUrl(String::new()),
    );
    assert_eq!(
        chip_from_engine(&engine, 480),
        None,
        "engine: empty history, no chip"
    );
}

/// CLASS (b) — spec-decided normalize, LANDED at stage (iii) (spec §3:
/// `liveness()` derives from the NEWEST frame's age; the pre-unify chip
/// aged the DISPLAYED volume and flagged a fresh feed STALE while the user
/// browsed an old frame). Both the unified `mode_chip_state` and the
/// suite's engine chip must now stay LIVE off the newest frame — pinned in
/// both directions so neither side can drift back to displayed-frame
/// aging.
#[test]
fn liveness_diff_displayed_vs_newest_age_is_the_spec_normalize() {
    let mut app = test_viewer_app_with_hazards(Vec::new());
    app.primary.live.enabled = true;
    let old_displayed = aged_volume("KEAX", 90_030); // ~25 h, browsed backward
    let fresh_newest = aged_volume("KEAX", 90);
    seed_primary_chip_frame(&mut app, &old_displayed);
    seed_primary_chip_frame(&mut app, &fresh_newest);
    app.volume = Some(Arc::clone(&old_displayed)); // browsing the old frame
    let unified = app
        .mode_chip_state()
        .map(|(label, _color, tag)| (label, tag));
    assert_eq!(
        unified,
        Some(("⏺ LIVE".to_owned(), "LIVE")),
        "the unified chip ages the NEWEST frame, not the browsed display (normalize landed)"
    );

    let mut engine = engine_with_volume(
        EngineRole::Primary,
        FeedSource::CustomUrl(String::new()),
        &old_displayed,
    );
    engine.history.push(FrameHistoryEntry {
        identity: frame_identity_for_volume(&fresh_newest),
        path: PathBuf::from("newest"),
        volume: fresh_newest,
        timings: None,
        status: FrameStatus::Complete,
        source_label: "newest".to_owned(),
    });
    engine.live.enabled = true;
    assert_eq!(
        chip_from_engine(&engine, 480),
        Some(("⏺ LIVE".to_owned(), "LIVE")),
        "engine ages the NEWEST frame (spec §3)"
    );
}

/// CLASS (b) — spec-decided normalize, LANDED at stage (iii) (spec §3
/// retired `pane_chip_is_live`'s pane-0 fall-through, the last R8 residue;
/// spec §8 row `realtime_level2_auto_refresh`). For an international
/// primary the pane-0 grid chip used to read the US auto-refresh flag and
/// claim ARCHIVE while the primary chip honestly said LIVE; both now share
/// the primary's display-owner-aware liveness, so the disagreement is
/// unrepresentable. The engine cell must agree.
#[test]
fn liveness_diff_pane0_fallthrough_r8_residue_is_pinned_until_adoption() {
    let intl_feed = FeedSource::Live(SiteRef::Intl {
        provider_id: "ord".to_owned(),
        site_id: "deess".to_owned(),
    });
    let mut app = test_viewer_app_with_hazards(Vec::new());
    app.primary.feed = intl_feed.clone();
    app.poll_active = true; // a genuinely live intl poll owns the primary
    app.primary.live.enabled = false; // the stale US flag — no intl path sets it
    let volume = aged_volume("deess", 90);
    seed_primary_chip_frame(&mut app, &volume);

    let primary_tag = app.mode_chip_state().map(|(_, _, tag)| tag);
    assert_eq!(primary_tag, Some("LIVE"), "the primary chip is intl-aware");
    let pane0_tag = app.pane_mode_chip_state(0).map(|(_, _, tag)| tag);
    assert_eq!(
        pane0_tag,
        Some("LIVE"),
        "R8 residue dead: pane 0 shares the primary's display-owner liveness"
    );

    let mut engine = engine_with_volume(EngineRole::Primary, intl_feed, &volume);
    engine.live.enabled = true; // the engine cell carries the real liveness
    assert_eq!(
        chip_from_engine(&engine, 480).map(|(_, tag)| tag),
        Some("LIVE"),
        "the engine cell agrees with both chips"
    );
}

/// Sanity tie-back for the pane role (adopted at 4d): the independent
/// radar-owning pane's chip truth (`pane_chip_is_live`) agrees with the
/// pane ENGINE's liveness — LIVE only while the pane engine's Live toggle
/// is on. Uses an international pin so the classification needs no US
/// catalog (the test app's `sites` list is empty by design).
#[test]
fn liveness_diff_independent_pane_chip_agrees_with_pane_engine() {
    let mut app = test_viewer_app_with_hazards(Vec::new());
    let volume = aged_volume("deess", 90);
    let mut pane = test_pane(ref_product());
    pane.pin = Some(SiteRef::Intl {
        provider_id: "ord".to_owned(),
        site_id: "deess".to_owned(),
    });
    pane.sync_engine_feed_to_pin();
    pane.engine.live.enabled = true;
    pane.engine.history.push(FrameHistoryEntry {
        identity: frame_identity_for_volume(&volume),
        path: PathBuf::from("pane-cell"),
        volume: Arc::clone(&volume),
        timings: None,
        status: FrameStatus::Complete,
        source_label: "pane cell".to_owned(),
    });
    app.extra_panes.push(pane);

    assert!(app.pane_chip_is_live(1), "pinned live pane claims LIVE");
    assert!(
        matches!(
            app.extra_panes[0].engine.liveness(Utc::now(), 480),
            Some(Liveness::Live { .. })
        ),
        "the pane engine agrees"
    );

    // Toggle off: both sides read archive.
    app.extra_panes[0].engine.live.enabled = false;
    assert!(!app.pane_chip_is_live(1));
    assert!(matches!(
        app.extra_panes[0].engine.liveness(Utc::now(), 480),
        Some(Liveness::Archive { .. })
    ));
}

/// The US 1 s chunk chain stays OUT of this suite (spec §9): the engine
/// documents the primary chunk cadence but must not become its source
/// until the re-point passes this gate; secondary roles never poll on it.
#[test]
fn liveness_diff_us_chunk_chain_stays_out_of_scope() {
    let pane_engine = LoopEngine::new(
        EngineId(48),
        EngineRole::Pane { slot: 0 },
        FeedSource::Live(SiteRef::Us {
            level2_id: "KEAX".to_owned(),
        }),
    );
    let primary_engine = LoopEngine::new(
        EngineId(49),
        EngineRole::Primary,
        FeedSource::Live(SiteRef::Us {
            level2_id: "KEAX".to_owned(),
        }),
    );
    assert!(pane_engine.poll_cadence() > primary_engine.poll_cadence());
    assert_eq!(
        primary_engine.poll_cadence(),
        Some(std::time::Duration::from_secs(1))
    );
}
