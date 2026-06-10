# BowEcho Storm-Cell Tracking Rebuild — Implementation Spec

Replaces: `C:\Users\drew\radar-work\bowecho\crates\render2d\src\cells.rs` (multi-threshold CC, 40/45/50/55 dBZ, 8 km², overlap dedup, linear-Z centroids) and the tracking half of `C:\Users\drew\radar-work\bowecho\crates\app_ui\src\main.rs` (`StormTrack` ~L1110–1166, `associate_storm_cells` ~L7528–7590: greedy nearest-to-prediction, flat 16 km gate, replace-last-fix on partial volumes, drop at 2 missed).

Root causes of the QLCS failure, mapped to literature: (a) greedy per-track matching steals neighbors when cell spacing (~10–15 km) is comparable to per-volume displacement (25–35 m/s × 300 s ≈ 9 km) — exactly the regime TITAN's global assignment was built for (Dixon & Wiener 1993, J. Atmos. Oceanic Technol. 10(6), 785–797, §3a); (b) single-pass CC at 40 dBZ glues line cells into one blob, so detections appear/vanish/recombine frame to frame (Han et al. 2009 "ETITAN", JTECH 26(4), 719–732, motivation; Lakshmanan, Hondl & Rabin 2009, JTECH 26(3), 523–537); (c) replace-last-fix on live partial composites corrupts histories; (d) hard-killing the motion fit at >60 m/s leaves tracks gateless.

New code layout: identification stays in `render2d` (rewrite `cells.rs`, new `watershed.rs`); association + motion move from `main.rs` into a new pure module `crates/app_ui/src/tracking.rs` (no egui deps — required for the unit tests in §5). `main.rs` keeps only the channel plumbing and drawing.

---

## 1. CELL IDENTIFICATION — replace multi-threshold CC with the enhanced watershed

**Decision: change.** Multi-threshold CC cannot split two cores inside one contiguous ≥40 dBZ QLCS envelope until some higher global threshold happens to separate them; the enhanced watershed (Lakshmanan, Hondl & Rabin 2009, JTECH 26(3), 523–537 — the WDSS-II `w2localmax` identifier) tests every threshold in one pass with a per-cell local hysteresis level, splits adjacent cores, finds initiating cells without ballooning mature ones, and produces frame-to-frame-stable sizes — which is what makes the √(A/π) association radius in §2 work. It is also the identifier Lakshmanan & Smith (2010, Wea. Forecasting 25(2), 721–729) chose as the common base for fair tracker comparison. Cost class ≈ 2–4× connected-components labeling: counting-sort bucketing O(N) + pre-pruned maxima + BFS basin growth that touches each successful-basin pixel once; failed basins are < saliency px by definition, bounding retries. On the 460×1200 ≈ 552K-px composite this is low-single-digit ms in optimized Rust (they report ~0.6 s wall for 18 Mpx on a 1 GHz 2009 Opteron) — inside the 10 ms budget including the smooth.

Pipeline (run on the existing composite-reflectivity polar grid, range-capped at `MAX_RANGE_M = 300_000.0` as today):

1. **Smooth**: separable Gaussian, σ = 1.5 px, 7-tap, wrapping in azimuth, on the dBZ grid (NaN-aware: renormalize kernel over valid gates). Lakshmanan et al. 2009 §"Smoothing" (their NEXRAD config used Gaussian σ = 3 km on Cartesian; Lakshmanan & Smith 2010 used a 9×9 median — Gaussian chosen here for speed; revisit with §6 metrics).
2. **Quantize** (their Eq. 2): `Q = 0` for Z ≤ a; `round((Z − a)/δ)` for a < Z ≤ b; `round((b−a)/δ)` above. **a = 30 dBZ, b = 60 dBZ, δ = 1 dBZ → 31 levels (maxlevel = 30).** The 30 dBZ floor + 20 km² minimum is the cell definition of Lakshmanan & Smith 2010 (footnote 1) and the bottom rung of the SCIT ladder (Johnson et al. 1998, Wea. Forecasting 13(2), 263–276, App. A #12: 30/35/40/45/50/55/60 dBZ). The quantization ladder IS the threshold ladder — all 31 rungs in one pass instead of SCIT's one-pass-per-threshold.
3. **Bucket** pixels by level (counting-sort layout, their Procedure 1); find candidate maxima in reverse intensity order, suppressing 8-neighbors of already-accepted candidates (their pre-pruning, ≈8× speedup).
4. **Immersion** (their Procedures 2–3): `for depth in 0..=MAX_DEPTH`, `for level in (0..=maxlevel).rev()`, `hlevel = level − depth`; for each unlabeled center at `level`, BFS-capture all contiguous (8-connected, azimuth-wrapping) pixels with Q ≥ hlevel. **MAX_DEPTH = 8 levels (8 dBZ)** — the per-cell hysteresis bound (tunable 5–10 via §6). **Saliency: accumulate true gate area `(range·Δaz)·Δr / 1e6` during growth (polar-grid adaptation of their pixel-count saliency) and capture iff area ≥ `MIN_CELL_AREA_KM2 = 20.0`** (Lakshmanan & Smith 2010 footnote 1; replaces today's 8 km², which admits speckle). Failed basins re-queue their center at level−1. Reserve foothills around each captured basin so neighboring centers can't leak into it.
5. **Dedup rule: none needed** — basins are disjoint by construction, which deletes the current fragile nested-overlap rule outright. (If the CC fallback below is ever used, the dedup rule is SCIT's core extraction: discard a lower-threshold component when a higher-threshold component's centroid falls inside it — Johnson et al. 1998 §2b, Fig. 4.)
6. **Centroid weighting**: per-gate weight `w = gate_area_km2 × Z_linear^(4/7)` with `Z_linear = 10^(dBZ/10)` — the Greene & Clark (1972) liquid-water relation SCIT uses for component mass and VIL (Johnson et al. 1998 §2b p. 265, App. B Eq. B1: m ∝ 3.44×10⁻⁶ Z^(4/7)). Replaces the current raw linear-Z weighting, which over-concentrates the centroid on the peak gate and makes it jitter; the 4/7 exponent is the documented SCIT mass weighting.
7. **Output** `StormCell` (extend the struct): `east_km, north_km` (mass-weighted centroid), `area_km2`, `max_dbz` (peak smoothed dBZ), `eq_radius_km = sqrt(area_km2/π)`, `mass = Σ w` (Z^(4/7)-weighted, association consistency attribute), `hlevel_dbz` (replaces `level_dbz`). Rank by `max_dbz` descending, truncate at **MAX_CELLS = 32** (raised from 20 — a mature QLCS line legitimately carries >20 cells).

**Fallback (only if profiling fails the 10 ms budget — not expected):** multi-threshold hysteresis CC with the full SCIT ladder 30/35/40/45/50/55/60 dBZ (Johnson et al. 1998 App. A #12), min component area 10 km² per threshold (App. A #2, default 10.0 km², range 10–30), ≥2-segment / clutter screen approximated by the area floor, SCIT core-extraction dedup as above, same Z^(4/7) centroids. Two-pass union-find CCL ≈ 1–3 ms at 7 thresholds.

## 2. ASSOCIATION — TITAN global assignment with SCIT first guess and overlap split/merge

Runs in `tracking.rs::associate(tracks: &mut Vec<StormTrack>, time, cells: &[StormCell])`, on the background thread, once per *completed* volume (§4). All ≤32×32, so everything below is microseconds.

**Step 0 — outage gate.** If Δt since the last associated volume > **TIME_GATE = 20 min**, do not associate: clear all tracks and seed fresh ones (Johnson et al. 1998 §2c, App. A #16: TIME = 20 min, range 10–60). Site change clears too (as today).

**Step 1 — first guess.** Predicted position of track i: `p_i = last_pos + motion_i · Δt`, with `motion_i` resolved by the SCIT fallback chain (Johnson et al. 1998 §2c): (a) the track's own fitted motion (§3); else (b) the **default motion vector = mean of all fitted track motions from the previous volume**; else (c) the app's user-set storm motion (`storm_motion_direction_deg/speed_kt` — SCIT's "user input (SPEED, DIRECTION)" fallback; SCIT's own climatological default is 225°/30 m s⁻¹, App. A #4/#15); else (d) zero motion with the enlarged gate below. Record which rung was used as `assumed_motion` (kept separate from `fitted_motion`; never enters the LSQ fit). *v2 enhancement, cited for the code comment but out of scope now:* FFT phase-correlation first guess per TINT (Raut et al. 2021, JAMC 60(4), 513–526) / ETITAN's cross-correlation dynamic constraint (Han et al. 2009, JTECH 26(4), 719–732).

**Step 2 — feasibility gate (speed-dependent search radius).** Pair (track i, cell j) is feasible iff

`d_p(i,j) = ‖p_i − x_j‖ ≤ R(i,j) = v_gate(i) · Δt + eq_radius_km(j)`, with `v_gate(i) = 15 m/s` if track i has a fitted motion (residual gate after advection; TITAN's s_max = 60 km h⁻¹ ≈ 16.7 m s⁻¹, Dixon & Wiener 1993 Eq. 4) and `v_gate(i) = 30 m/s` otherwise (SCIT SPEED, App. A #15: "speed used to compute the correlation distance", i.e. threshold = SPEED × Δt — this is the prompt's "enlarged radius" for motionless tracks), floored at `R ≥ 5 km`. The `+ eq_radius_km(j)` size term is the OC/Han et al. (2009) and Lakshmanan & Smith (2010, step 3) √(A/π) search radius — big cells may match farther. At Δt = 300 s: fitted track gate = 4.5 km + r_eq (residual after advection — tight, prevents steals); new track gate = 9 km + r_eq. Infeasible pairs get cost = BIG (1e12 after integer scaling), excluded per Dixon & Wiener 1993 Eq. 4.

**Step 3 — uniqueness pre-pass (isolated storms bypass the cost function).** Lakshmanan & Smith 2010, their devised algorithm steps 1–5 (their consistently best method): sort tracks by **descending track length** (longevity first — their AGE finding: "the key parameters for a tracking algorithm are location error and longevity"); for each unassociated track, collect feasible cells within `d = eq_radius_km(track's last cell)`; if **exactly one** candidate lies in that radius **and** `d_p ≤ 5.0 km`, associate immediately. Iterate until no change.

**Step 4 — global assignment on the remainder (Hungarian, not greedy).** Cost (TITAN Eq. 1–3 adapted to a composite-only 2-D pipeline, plus the intensity-consistency term of Lakshmanan & Smith 2010 Eq. 1):

`C_ij = w1·d_p + w2·|eq_radius_i − eq_radius_j| + w3·|max_dbz_i − max_dbz_j|`

- `w1 = w2 = 1.0` (TITAN's study weights, Dixon & Wiener 1993 §3a; their d_v = |V₁^⅓ − V₂^⅓| volume term becomes the equivalent-radius difference |√(A_i/π) − √(A_j/π)| in km — same commensurate-distance-units trick, one dimension down).
- `w3 = 0.2 km/dBZ` (10 dBZ peak mismatch ≡ 2 km). Rationale: L&S 2010 found pure size-consistency costs (CST/OC) "overemphasize size preservation" and produce the worst mismatches, and they recommend peak/mean composite reflectivity — NOT size — as the consistency attribute when VIL is unavailable. w3 is ours; tune with §6 metrics only.

Solve the rectangular assignment padded to n×n, n = max(#tracks, #cells) ≤ 32, pad entries = 10× the max finite cost so unmatched rows/columns map to dummies — **track births and deaths fall out of the assignment automatically** (Dixon & Wiener 1993 §3a; Hungarian/Munkres, complexity O(p²q), their Eq. 5 — at n ≤ 32 that is ≤ ~33K ops, microseconds). Implementation: `pathfinding::kuhn_munkres` on an i64 matrix with costs scaled ×1000 (metres), or a self-contained ~100-line Munkres. Greedy-on-sorted-costs is explicitly rejected: sequential nearest-neighbor steals matches in clusters — the documented QLCS failure mode (Dixon & Wiener 1993 §3a assumptions i–iii; L&S 2010 found SCIT-style PRJ loses track duration in 2/5 case types). The uniqueness pre-pass already gives isolated cells the cheap path.

**Step 5 — split/merge post-pass (overlap-based, TITAN §3b adapted to equivalent-radius circles since we carry no ellipse):**
- **Merge:** for each track left UNMATCHED by steps 3–4, advect its centroid to `time`; if it lands within `eq_radius_km` of a *matched* cell, mark the track `merged_into = surviving_track_id` and terminate it immediately (no coasting, no extrapolation dots). The survivor is whichever parent the assignment extended; if assignment extended none, the **longest-lived** feasible parent inherits the merged cell's track (ID-propagation choice per L&S 2010 split/merge discussion: propagating the longer-lived ID lengthens duration). Do not implement TITAN's history translation/volume-weighted averaging in v1.
- **Split:** for each cell left UNMATCHED (about to become a new track), check all tracks (matched ones included): if the cell's centroid lies within track's forecast circle (advected centroid, radius = `eq_radius_km` of the track's last cell, area held constant — TITAN Eq. 7 degenerates to persistence without volume trends), mark the new track `parent_id = track.id` and **copy the parent's motion vector into the child's `assumed_motion`** (TITAN: each split child receives the parent history; we transfer only the motion — enough for the next gate and the extrapolation cone). Critical for QLCS: a bowing segment that splits must not start cold at zero motion.

**Step 6 — bookkeeping.**
- Matched track: push `(time, east_km, north_km)` (history `VecDeque`, cap **10** — SCIT keeps up to 10 past volumes, Johnson et al. 1998 §2c, Table 1), update `max_dbz`, `missed = 0`, refit motion (§3).
- Unmatched track: `missed += 1`; **coast while `missed ≤ COAST_VOLUMES = 2`** (retain, advect for display and next first guess, add NO history point), drop at 3. SCIT itself never coasts (§2c — unmatched cells simply end) and Lakshmanan & Smith (2008) coast 3 frames; 2 bridges single-volume identification dropouts at QLCS flicker without zombie tracks. Tunable by the §6 duration metric ("search radius and coasting count are tunable by the same metrics" — L&S 2010). Current code's drop-at-2-missed becomes drop-at-3.
- Unmatched cell: new track, `id = next_id`, one-point history, `fitted_motion = None`, `assumed_motion` from Step 1 chain (b)/(c) or Step 5 split inheritance.

## 3. MOTION ESTIMATE

`refit_motion()` in `tracking.rs`, called on every successful association:

- **Window:** the most recent `min(len, 6)` fixes (TITAN n_t = 6 scans, Dixon & Wiener 1993 §4; SCIT fits up to 10 with equal weights, Johnson et al. 1998 §2c — we keep the 10-deep history for display/§6 but fit on 6).
- **Fit:** weighted linear least squares of east(t) and north(t) vs t, **weights w_k = α^k, α = 0.5, k = 0 = newest** (TITAN's linear trend with double exponential smoothing, Abraham & Ledolter 1983, via Dixon & Wiener 1993 §4; their Table 5 shows forecast skill flat for α ∈ 0.25–0.75, so 0.5 is safe and tracks QLCS acceleration far better than SCIT's equal weights). The track's *position* is always the latest observation — only the trend is smoothed (TITAN: "the current value is taken as correct").
- **Guards:** require ≥2 distinct fixes spanning ≥ 60 s (else `fitted_motion = None`); denominator `|Σw·Σwt² − (Σwt)²| > 1e−6` else None.
- **Outlier rejection (one pass):** after the fit, if exactly one point has residual > max(5 km, 2.5× the RMS residual), refit without it (history keeps the point; only the fit excludes it). Protects the vector from a single residual mis-association.
- **Speed sanity:** if fitted |v| > **60 m/s**, keep the *previous* `fitted_motion` and set a `suspect` flag — do NOT null the motion (the current code's `motion_mps = None` on violation collapses the gate to the 30 m/s no-motion radius and is one of the QLCS breakers). If two consecutive fits violate, then null it.
- Extrapolation dots (+15/+30/+45 min, unchanged UI): drawn only from `fitted_motion` (≥2 fixes). Forecast-error yardstick for sanity: SCIT Table 6 — 5.0 km at 15 min, 9.9 km at 30, 15.2 km at 45 (Johnson et al. 1998 §3).

## 4. LIVE-PARTIAL POLICY

**Abolish replace-last-fix.** Partial composites yield partial cells (centroids biased toward whichever tilts have arrived; max-in-column underestimates), and rewriting history points poisons the LSQ fit. New rule:

- **Track only on volumes whose composite is complete enough.** `trackable(volume)` iff the lowest reflectivity tilt is present AND ( the volume carries an end-of-volume status, OR ≥ 80% of the VCP's expected reflectivity cuts have arrived, OR the highest available REF elevation ≥ 6.5° — beyond ~6.5° additional tilts change the composite only very near the radar ). This matches SCIT's contract: detection runs after "the last radial of an elevation scan" and association on complete volume scans (Johnson et al. 1998 §2b–c).
- **At most one association per `volume_time`, monotonically increasing.** Once a volume time is associated, later refinements of the same volume are ignored for tracking (identification may still rerun for the on-screen cell markers — drawing live cells is fine; mutating track state is not).
- Live partial volumes: draw current cells + advected track positions (using existing motions); never touch histories.
- Δt for gates and fits = difference of volume start times. SAILS/MESO-SAILS mid-volume low-tilt insertions are naturally handled because completion, not chunk arrival, triggers tracking.

## 5. UNIT TESTS (in `tracking.rs`; synthetic `StormCell` lists, no radar I/O)

1. **Crossing cells must not swap IDs** (mismatch class, Lakshmanan & Smith 2010 Fig. 1). Two cells on crossing lines (headings 090° and 045°, both 25 m/s), Δt = 300 s, 6 volumes, closest approach < 5 km at volume 4; cell A: 300 km²/60 dBZ, cell B: 80 km²/45 dBZ. Assert both track IDs survive all 6 volumes and each track's final position lies on its own line (per-track line-fit RMSE < 1 km). Greedy-nearest swaps here; Hungarian + size/intensity terms must not.
2. **QLCS no-steal.** Five cells in a line, 12 km spacing, all moving 30 m/s from 240°, Δt = 300 s (displacement 9 km ≈ spacing — the field-report regime), 4 volumes. Assert i→i mapping every volume (5 tracks, no births/deaths after volume 1, zero ID churn). This is the TITAN §3a global-optimization claim, and the case the flat-16-km greedy demonstrably breaks.
3. **A split must link both children to the parent** (Dixon & Wiener 1993 §3b). Parent 200 km² moving (20, 0) m/s; next volume: two 100 km² cells at the forecast position ±4 km lateral. Assert: one child continues the parent track ID; the other becomes a new track with `parent_id = parent` AND `assumed_motion ≈ (20, 0) m/s` (not zero).
4. **A merge must terminate the losing parent with a link, not let it ghost.** Two parents converging at 20 m/s; next volume one 250 km² cell at the midpoint forecast. Assert: exactly one surviving track (the assignment winner / longer-lived parent), the other marked `merged_into = survivor_id` and removed from coasting; no spurious new track.
5. **Speed gate must reject a teleporting cell** (Dixon & Wiener 1993 Eq. 4). Track with fitted motion (25, 0) m/s; only candidate cell 35 km north of the prediction at Δt = 300 s (residual 117 m/s ≫ 15 m/s gate). Assert: no association; track coasts (`missed = 1`, retained, history unchanged); the cell starts a fresh track.
6. **Coast-and-reacquire.** Cell at volumes 1–2, absent at 3 (identification dropout), present at 4 within 2 km of the extrapolated position. Assert same track ID at volume 4, history has exactly 3 points (no fabricated volume-3 fix). (Coast count per Lakshmanan & Smith 2008 precedent.)
7. **TIME gate.** Δt = 25 min > 20 min (Johnson et al. 1998 App. A #16): assert no association is attempted and all prior tracks are dropped; all current cells become new tracks.
8. **Identification — watershed splits a QLCS blob** (in `render2d`): synthetic polar grid with two 55 dBZ Gaussian cores 8 km apart inside one contiguous 42 dBZ envelope. Assert `identify_storm_cells` returns **two** cells with centroids within 1.5 km of each core (the current 40-dBZ CC returns one — the ETITAN/Han et al. 2009 motivating case). Keep the three existing tests in cells.rs (single 55 dBZ blob found at the right place; sub-30 dBZ echo → none; empty volume → none), with the area threshold updated to 20 km².

## 6. VALIDATION METRICS — Lakshmanan & Smith (2010, Wea. Forecasting 25(2), 721–729; note: 721–729, not 701–709)

Run on a recorded ≥6-volume real QLCS sequence (the archived KMBX bow-echo case; the KEAX 2026-06-09 derecho scans also qualify). **Protocol — vary only association:** serialize the per-volume `Vec<StormCell>` from the new identifier once, then feed the identical cell streams to (a) the current greedy tracker and (b) the new tracker; identical first-guess motion configuration. Their explicit rule: identification and motion held fixed, association compared; numbers are *relative on the same data*, never absolute. Do **not** collapse to one score ("the temptation ought to be resisted").

1. **Median track duration `~dur`** — median (not mean — robust to 1-frame fragments and outlier long-livers) over ALL tracks of `last_time − first_time` (seconds). Longer median ⇔ fewer dropped associations. Sanity bar: "a technique where the median track has less than three centroids is not worth evaluating" — at Δt ≈ 5 min the median must exceed ~600 s.
2. **Mismatch error `mean(σ_Z)`** — for each track with `dur > ~dur` (longest 50% only — σ on short tracks is noise), the standard deviation along the track of **max composite reflectivity** (their attribute is VIL; with a composite-only pipeline use peak reflectivity, NOT size — size as the consistency attribute biases the metric toward cost-function trackers, their caveat 9). Mean over those tracks. Lower ⇔ fewer identity swaps.
3. **Linearity error `mean(e_xy)`** — for each track with `dur > ~dur`, the RMSE (km) of centroid positions about their best-fit line in the x–y plane (paper says only "optimal line fit"; implementation choice to document: total-least-squares line through the (x,y) sequence, orthogonal residuals). Mean over those tracks. Lower ⇔ fewer jumps (decayed-cell→new-cell ID leaps bend tracks).

Error bars at 50% confidence (ranking, not proof — their choice): means → μ ± 0.67·σ/√N over the N tracks used; median → order statistics, durations ranked N/2 ± 0.67·√(N/4) (Conover 1980, Practical Nonparametric Statistics).

**"Better" =** on the same QLCS sequence the new tracker must show (i) `~dur` strictly larger with non-overlapping 50% CIs — given the field report ("tracks generally broken"), target ≥ 2× the greedy baseline; (ii) `mean(e_xy)` ≤ baseline (a jump-prone tracker buys duration with bent tracks — this is the balance check); (iii) `mean(σ_Z)` ≤ baseline. Tuning recipe (theirs, verbatim logic): raise the gate speed / coast count only while `~dur` keeps increasing AND `mean(σ_Z)`, `mean(e_xy)` stay at or below the greedy-PRJ baseline. Note for the report: 6 volumes is short — rankings hold for this case and scale (20 km² cells) only.

## Constants table (single source of truth, `tracking.rs::params` + `cells.rs`)

| Constant | Value | Source |
|---|---|---|
| Quantize a/b/δ | 30 / 60 / 1 dBZ | Lakshmanan et al. 2009 Eq. 2; L&S 2010 fn. 1; SCIT ladder App. A #12 |
| MAX_DEPTH (hysteresis) | 8 dBZ | Lakshmanan et al. 2009 ("bounded by a max depth"); tune via §6 |
| MIN_CELL_AREA_KM2 (saliency) | 20.0 km² | L&S 2010 fn. 1 |
| Gaussian smooth σ | 1.5 px | Lakshmanan et al. 2009 smoothing stage |
| MAX_CELLS / MAX_RANGE | 32 / 300 km | app |
| Centroid weight | area · Z_lin^(4/7) | Greene & Clark 1972 via Johnson et al. 1998 App. B Eq. B1 |
| TIME_GATE | 20 min | Johnson et al. 1998 App. A #16 |
| v_gate fitted / unfitted | 15 / 30 m/s | Dixon & Wiener 1993 Eq. 4 (s_max = 60 km/h, residual); Johnson et al. 1998 App. A #15 (SPEED) |
| Gate radius | v_gate·Δt + √(A_cell/π), floor 5 km | Han et al. 2009; L&S 2010 steps 3–4 |
| Cost weights w1/w2/w3 | 1.0 / 1.0 / 0.2 km/dBZ | Dixon & Wiener 1993 §3a (w1=w2=1); w3 ours per L&S 2010 intensity-consistency finding |
| Assignment | Hungarian, padded n×n, n ≤ 32, pad = 10×max cost | Dixon & Wiener 1993 §3a, Eq. 5 |
| Uniqueness pre-pass | 1 candidate in √(A/π) AND d ≤ 5 km | L&S 2010 devised algorithm step 4 |
| History cap / fit window | 10 / 6 fixes | Johnson et al. 1998 §2c (≤10); Dixon & Wiener 1993 §4 (n_t = 6) |
| Fit weights | α^k, α = 0.5 | Dixon & Wiener 1993 §4, Table 5; Abraham & Ledolter 1983 |
| Speed sanity | 60 m/s → keep previous fit, flag; null after 2 | app (was: null immediately) |
| COAST_VOLUMES | 2 | between SCIT's 0 (Johnson et al. 1998 §2c) and L&S 2008's 3; tune via §6 |
| Volume completeness | lowest tilt + (EOV ∨ ≥80% REF cuts ∨ top REF tilt ≥ 6.5°) | §4 |

Performance budget check: identification (smooth + quantize + bucket + immersion) ≈ 3–6 ms on 552K px — within the 10 ms target; association ≤ 32³ Hungarian + pre/post passes ≪ 1 ms on the background thread.