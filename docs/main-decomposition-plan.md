# Decomposing main.rs — the v0.30 plan

*Written 2026-07-07 against `2150db4` (v0.29.3). Recon facts: `radar-work/refactor-recon-notes.md`.*

## Why (measured, not vibes)

- `crates/app_ui/src/main.rs` is **79,177 lines** — 56% of app_ui's hand-written source. One `ViewerApp` struct with ~330 fields, ~700 methods, ~630 free functions, and a single 24k-line test module.
- Since June 1, **68% of all commits touched main.rs** (7.6× the busiest sibling module). During the v0.29.3 cycle every parallel agent collided there; three tracks independently fixed the same lint sites and the integrator resolved 19+ merge conflicts that existed only because the code shares one file.
- The goal of v0.30 phase 1 is NOT architecture — it is **making parallel work collide-free and navigation sane**, with zero behavior risk.

## Non-goals (v0.30)

- **No crate split.** The proven extraction pattern is `impl ViewerApp` blocks in sibling files, which the orphan rule confines to the same crate. A multi-crate split needs a ViewerApp-holding core crate and untangling of the loop-engine web — deferred until there's a compile-time reason strong enough to pay for it. (Iteration builds already have `release-fast`: thin LTO, 16 CGU, incremental.)
- **No behavior changes, no renames, no restructuring** inside phase-1 commits. Ever.

## The safety protocol — "move, don't modify"

Every phase-1 commit is a *mechanical move* whose correctness is machine-checkable:

1. **Verbatim relocation.** Function/impl bodies move byte-identical. The only permitted edits: `use` lines, `mod` declarations, and `fn` → `pub(crate) fn` visibility promotions (each one listed in the commit message).
2. **Tests move with their code.** The moved functions' `#[test]` fns leave the main.rs test block and land in the new module's own `#[cfg(test)] mod tests`. A moved test that stops compiling is a signal the move was not mechanical — stop and look.
3. **Review with `git diff --color-moved=dimmed-zebra`.** Pure moves render as dimmed moved blocks; ANY in-body edit lights up. The diff itself is the QA evidence — no manual retest of the feature is required for a clean move-only diff.
4. **Full gates per commit** on the build nodes: `fmt --all --check`, `clippy --workspace --all-targets -- -D warnings`, `test --workspace` (1,790+ tests). Identical pass/fail count before and after.
5. **One extraction per commit**, revertable in isolation. No extraction depends on another landing first.
6. **Historical ratchet (currently discontinued):** phase 1 used a checked-in line ceiling to keep extraction moving. The owner discontinued that test on 2026-07-09. Cohesive module ownership remains preferred, but line count is no longer a gate and must not block a correct in-place fix.

If any step deviates from mechanical (a borrow-checker fight, a needed signature change), that extraction is **paused and split**: land the mechanical part, open the semantic part as its own reviewed change. Never mix.

## Which pattern for which code (all three already proven in-repo)

| Code shape | Pattern | Existing template |
|---|---|---|
| Pure functions (params in, value out) | Free fns in a new module, `pub(crate)` | `brand.rs`, `model_data.rs` |
| Feature with cohesive state + a few paint/poll methods | Owned `FeatureState` sub-struct + small `impl ViewerApp` block in the module | `tropical.rs` (the model), `tor_tracks.rs`, `annotate.rs` |
| UI panels needing broad `&mut self` | `impl ViewerApp` methods in a sibling file | `layers_rail.rs`, `overlays.rs` |

## Phase 1 — the extraction queue (in order)

| # | New module | Source region (main.rs @ 2150db4) | ~Lines | Risk notes |
|---|---|---|---|---|
| 1 | `self_update.rs` | 47916–48407 free fns + 3 thin methods (20228, 20277, 21767) | ~500 + tests | Leafiest in the file; ~30 pure fns, zero ViewerApp coupling |
| 2 | `hazard_geom.rs` | pure hazard geometry/parsing/fill fns within 48614–52290 | ~3,700 + tests | All params-only (`&HazardRecord`/`&[HazardPoint]`/`&str` → value); the heavily-tested v0.29.3 scanline/earcut code comes along |
| 3 | `hazard_ui.rs` | hazard panel/paint methods (25522–26559, 39782–40078) | ~1,300 | `impl ViewerApp`; hazard field cluster is self-contained |
| 4 | `product_select.rs` | 52289–53230 | ~950 + tests | Pure `(volume, …) → value`; big colocated test set |
| 5 | `sat_paint.rs` | sat methods 31397–34574 + sampling fns 54320–55055 | ~3,900 | Sat state already owned (SatWorker/panels/caches); fresh in memory from v0.29.3 |
| 6 | `geo_helpers.rs` | pure geometry scattered through 42896–47700 (haversine, lerp, graticule, bounds) | ~800 | Trivial params-only moves |
| 7 | `settings_ui.rs` | settings window panels 20303–22510 | ~2,200 | layers_rail pattern; broad-not-deep coupling |
| 8 | `map_paint.rs` | the G2 painting block 27603–42893 | ~15,000 | The big one. Cohesive but huge; do LAST in phase 1, in 3–4 sub-moves (markers / layers / chrome / projection), and only after the LoopEngine field migration (struct doc at 2283) stops renaming loop state |

Projected: main.rs shrinks ~28k lines (~35%) through step 8, and — more importantly — the five busiest editing fronts (satellite, hazards, settings, painting, updates) each get their own file.

**PHASE 1 COMPLETE (2026-07-08).** All 8 queue items landed. Item 8 (`map_paint.rs`) extracted in 4 move-only sub-moves — A projection, B chrome, C markers, D layers (recon: `radar-work/map-paint-recon-notes.md`); LoopEngine Phase-4e blocker was confirmed cleared first. `single_pane_canvas`/`grid_canvas` (input+dispatch) and the `handle_*_click` cluster stay in main.rs by design — that forces the `pub(crate)` promotions and matches the sat_paint/hazard_ui precedent; feature painters (spc/mping/glm/obs/model/storm) were excluded to keep their own module homes. Net: **main.rs 79,177 → 63,621 lines (−15,556, −19.6%)**, `map_paint.rs` = 4,449 lines. Every extraction gated move-only at exact test parity. The phase-1 line-count ratchet was later discontinued by the owner on 2026-07-09.

## Phase 2 (separate approval) — state absorption

Finish the in-progress migration of loose `primary.*` loop fields into `LoopEngine`/`PaneView` (the struct's own doc marks this Phase-4e). Only after that is the radar-loading/loop glue (impl #1, 10.6k lines) extractable. These commits touch real logic — they get feature-level review, not just move-review.

## Phase 3 (unscheduled) — only if ever needed

Crate split behind a `ViewerApp`-core crate for compile-time wins. Not planned; revisit if phase 1+2 plus `release-fast` still leave iteration too slow.

## Working agreement (starts now)

- Prefer new modules for genuinely cohesive ownership; do not create artificial modules solely to satisfy a line count. No main.rs line-count test is currently enforced.
- Parallel agents each own distinct modules; the integrator refuses two concurrent tracks on the same new module.
- Extraction commits and feature commits never mix.
