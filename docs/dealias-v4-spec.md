# Dealias v4 — engine specification

Status: SPEC (no code yet). Owner ask: a dealias engine that is "literally the
best dealias algo in existence." This document treats that as a **measured
claim**: §10 defines the battery and the pass criteria that make it true or
false. Written 2026-07-02 against branch `v028/unslop`.

Scope: `crates/render2d` only (plus a `crates/bench` extension for the eval
harness). No `app_ui`/`ui_core` work; the environmental-wind input is a
caller-supplied struct so render2d never grows a data-fetching dependency.

Style contract (owner decision 12b, `docs/v029-engine-spec.md`): boring
explicit code, several small files instead of one clever one, doc comments
that state the invariant, behavior-pinning tests. Every method with a
research basis names its primary papers here **and** in the code comments —
the existing dealias modules are the template.

---

## 1. Current engine inventory (audit)

Three engines ship today. All share the same core segmentation.

### 1.1 v1 — region-based unfolder (`lib.rs`, `dealias_velocity_grid`)

Evidence used: **same-sweep boundary continuity only.**
Method (Jing & Wiener 1993, *JTECH* 10, 798–808; Helmus & Collis 2016,
Py-ART, *JORS* 4(1):e25; in the spirit of Feldmann et al. 2020 R2D2, *JTECH*
37, 2341–2356):

1. flood-fill connected regions where adjacent gates differ by
   < 0.5·Nyquist (`REGION_JOIN_FRAC`);
2. region-adjacency graph; each edge carries the consensus integer fold
   between the two regions (majority vote over shared boundary gate pairs);
3. resolve strongest-boundary-first through `FoldUnionFind` (a potential/
   weighted DSU that accumulates `k[region] − k[root]`);
4. anchor each connected group so its **largest region is fold 0**;
5. apply folds, despeckle (Holleman & Beekhuis 2003, *JTECH* 20, 443–453;
   Altube et al. 2017, *JTECH* 34, 1529–1543).

Canonical acceptance: KEAX 2026-06-09 05:51 UTC derecho, 21,458 → ≈196
residual fold boundaries (`docs/dealias-fold-branch-analysis.md`).

What it cannot know: **the absolute Nyquist branch of every connected
group** (boundary votes constrain only relative folds), and any relative
fold across a spurious/ambiguous edge. See §2.

### 1.2 v2 — tilt-cascade (`cascade.rs`, `dealias_velocity_grid_cascade`)

Evidence added: **top-down inter-tilt wind reference.** Higher tilts usually
carry higher Nyquist and less aliasing, so the volume is dealiased from the
top tilt down; each solved tilt yields a per-range-band zeroth-harmonic fit
v̂(az) = a·cos az + b·sin az (Browning & Wexler 1968, *J. Appl. Meteor.* 7,
105–113) used as the external reference (in the spirit of UNRAVEL's
reference checks, Louf et al. 2020, *JTECH* 37, 741–758) for group-branch
selection and per-region override on the tilt below
(`dealias_velocity_grid_with_reference`, step 5b in `lib.rs`).

### 1.3 v3 — hybrid 3-D + temporal (`hybrid.rs`, `dealias_velocity_grid_hybrid`)

Evidence added, per target tilt: (a) previous volume's matching elevation
(temporal, ≤15 min, same site — the 4DD idea, James & Houze 2001, *JTECH*
18, 1674–1683); (b) nearest current-volume tilt(s) above/below (≤3° away);
(c) upper-tilt harmonic fit as broad fallback. All references are mapped
once onto the target polar lattice (`DenseVelocityReference`), fused with
agreement logic (12 m/s window, harmonic tie-break), then:

- conservative whole-group and per-region branch overrides in the region
  solver (step 5c in `lib.rs`, margin+coverage gated), and
- a guarded per-gate **local branch proposal** pass
  (`refine_local_branch_patches`) that can pull a compact folded lobe out of
  a numerically-identical opposite-sign background — with a 3-of-5 direct
  neighbor coherence vote.

Supporting tilts are solved with the *plain region engine* (deliberate: a
full cascade there would feed circular evidence back).

### 1.4 Measured baselines (2026-07-02)

Machine: AMD Ryzen 9 9950X3D, 64 GB, Windows 11, `--release`, best-of-5 per
cut. Harness: `render2d --example velocity_bench` (region) plus a scratch
driver calling `dealias_velocity_grid_hybrid` per cut. "folds" = residual
fold-boundary pairs: adjacent finite 4-neighbor gate pairs with
|Δv| > 1.2·N_row (N = per-cut max Nyquist). Stage 0 (§11) re-baselines with
the checked-in harness so these numbers become the pinned reference.

KEAX20260609_055143 (derecho, prior volume 054454 supplied to hybrid),
super-res 0.44° tilt, 858,240 gates:

| engine | time (worst super-res tilt) | whole volume, all vel cuts | folds, 0.44° |
|---|---|---|---|
| region (v1) | 18–24 ms | 176 ms | 264 |
| hybrid (v3) | 76–146 ms | 955 ms (each tilt solved independently) | 527 |

KMBX branch probes (5×5-gate mean around the documented gates, lowest
velocity cut):

| volume / gate | raw | region | hybrid | truth |
|---|---|---|---|---|
| 234055, az 202°, 57.9 km | +22.7 | **−34.0** | **−34.0** | ≈ −33 (verified vs RadarScope) |
| 235423, az 339°, 20 km (blob) | +13.3 | +13.3 | **+13.3 (WRONG)** | inbound (≈ −39) |
| 235423, az 316°, 21 km | +7.0 | +7.0 | +7.0 (suspect) | inbound sector per field report |

Two hard findings, both load-bearing for v4:

1. **The hybrid fails the canonical KMBX 23:54 blob today.** The temporal
   prior inherits the same misbranch (GIGO, exactly as
   `dealias-fold-branch-analysis.md` predicted) and every self-derived
   reference is contaminated. Only an external absolute reference can decide
   this event.
2. **The naive fold-boundary count is gameable, and the hybrid's per-gate
   patches inflate it.** A field left *consistently wrong* (everything on
   one folded branch) scores ~0 boundaries; and hybrid's local patches leave
   discontinuity **rings** at patch edges (KEAX 0.44°: 264 → 527; KLIX 0.88°
   split cut: 51 → 1012; KTLX 10.0°: 209 → 750). The eval (§10) therefore
   never reads boundary count alone, and v4's repair gauntlet must close
   patch rings instead of creating them.

---

## 2. Failure-mode catalog (concrete)

Each mode names the storm morphology / Nyquist regime, the engines that
fail it, and the evidence.

- **F1. Group absolute-branch under-determination.** Any isolated connected
  group defaults to "largest region = fold 0". Wrong whenever the majority
  of a group is aliased: deep widespread aliasing in derecho rear-inflow
  jets, hurricane eyewalls, nocturnal LLJs under low Nyquist. Fails v1
  always (by construction); v2/v3 fix it only when their references are
  clean. Evidence: entire KMBX event class.
- **F2. Mega-region chaining.** Gradual noise chains one region across the
  full ±Nyquist span; genuinely-aliased patches join it and anchor at fold
  0, forcing correct neighbors to fold up. QLCS with broad stratiform
  shield. Fails v1; v2/v3 partially (interval splitting was tried and the
  bad branch survived the vote graph —
  `dealias-fold-branch-analysis.md` §root-cause 3).
- **F3. Vote-graph subgraph misbranch.** One spurious edge misbranches an
  internally-consistent subgraph; invisible to boundary evidence and to
  per-region mismatch checks (tested on the real volume, root-cause 4).
  KMBX 23:47–23:54, region 600 (826 gates, +1 fold → +29.1 m/s outbound,
  wrong). Fails v1/v2; v3's dense-reference override requires reference
  coverage + decisive margins, and its temporal reference inherits the same
  error (23:40 carries the same misbranch: az 316°, 21 km, raw −17.5 →
  +39.2, 16k gates — persistent, not transient).
- **F4. Same-sweep VAD circularity.** In widely-aliased sweeps, wrapped
  gates pass the |v| ≤ 0.7·Nyq reliability filter and poison the
  Browning & Wexler fit; 68/75 healthy-looking bands endorsed the wrong
  folds (root-cause 5). The reference is weakest exactly when needed most.
  Fails v1+VAD, v2 on single-tilt, v3's harmonic fallback.
- **F5. All-tilts-aliased VCPs.** KMBX ran Nyquist 26.2 m/s on every tilt
  to 6.6° (near-Nyquist fractions 10–20% all the way up); the only clean
  tilts (8–20°) sample different altitudes and fail the coverage gate. The
  cascade's core assumption (higher tilt = cleaner) is false here. Fails v2;
  v3's adjacent-tilt evidence is equally contaminated.
- **F6. Temporal GIGO / persistent misbranch.** v3 weights same-elevation
  temporal continuity strongest, but a persistent misbranch reproduces
  itself volume-to-volume. There is no confidence attached to the previous
  solution, so bad priors weigh as much as good ones. Fails v3.
- **F7. First-volume cold start.** No temporal prior exists (app start,
  site switch, dropped volumes >15 min). v3 degrades to F4/F5 territory in
  deep-aliasing events. Fails v1/v2/v3.
- **F8. Isolated folded pocket fused into an opposite-sign region.** A
  wrapped outbound lobe numerically equal to the surrounding inbound field
  fuses into one giant region — no boundary exists to vote on. v3's
  per-gate patch pass fixes the compact case when a dense reference covers
  it (pinned by `local_refinement_repairs_a_folded_patch_fused_into_
  legitimate_inbound`), but leaves **rings** at patch edges (§1.4 finding 2)
  and does nothing without reference coverage. Partially fails v3;
  fails v1/v2.
- **F9. Isolated cells with no context.** Lone convective cells at long
  range: one region, no neighbors, no vote edges. Branch = fold 0
  assumption. Aliased isolated cells (tropical outer bands, pulse severe
  inflow) come out wrong. Fails v1/v2; v3 only helps if another tilt or the
  previous volume saw the cell.
- **F10. Low-Nyquist regimes.** Clear-air VCPs (legacy VCP 31 long-pulse,
  N ≈ 11.5 m/s) under a strong LLJ alias the whole sweep with sparse echo;
  dual-PRF international feeds carry processor speckle (Holleman & Beekhuis
  2003); branch spacing 2N shrinks below v3's fixed margins
  (`LOCAL_BRANCH_MIN_IMPROVEMENT_MPS` 8 + separation 4 vs 2N ≈ 23), so the
  guarded passes go inert. Fails v1/v2/v3 to varying degrees. (JMA
  staggered-PRT feeds publish no Nyquist at all and are pass-through **by
  design** — `dealias_skipped_no_nyquist` — v4 keeps that invariant.)
- **F11. Legit shear misread as folds.** Tornadic mesocyclones / TVSs have
  gate-to-gate |Δv| approaching 2N. Continuity-only repair modules smooth
  them (the failure R2D2 was built around, Feldmann et al. 2020). The
  current engines' conservatism mostly protects couplets; any v4 repair
  gauntlet must protect them **explicitly**.
- **F12. Per-tilt independence.** v3 solves each displayed tilt separately
  (955 ms if the app ever asked for all of KEAX; supporting tilts re-solved
  per call, results can disagree tilt-to-tilt). No engine enforces volume
  consistency, and the work is not amortized.

---

## 3. State of the art (survey)

Primary citations; each maps to a v4 decision in §4.

- **Jing & Wiener 1993** (*JTECH* 10, 798–808). Two-dimensional dealiasing
  as a single global solve over the whole sweep (linear system on region
  fold numbers) instead of local gate-marching. Our v1 is this family; the
  WSR-88D ORPG's modern "2D VDA" derives from it (per Veillette et al.
  2023). Lesson kept: global > sequential.
- **Eilts & Smith 1990** (*JTECH* 7, 118–128). The legacy WSR-88D VDA:
  gate-continuity with **environmental wind constraints** as backup
  reference. Earliest operational precedent for an external wind anchor.
- **James & Houze 2001, 4DD** (*JTECH* 18, 1674–1683). Four-dimensional
  dealiasing: initialize from the **previous volume**, else a **VAD or
  environmental sounding**, then spatial continuity, flagging gates it
  cannot decide. v3's temporal reference is 4DD-shaped; 4DD's *sounding
  initialization* is the part we never built and the part that decides
  KMBX-class events. Known weakness (motivates our guards): errors in the
  initial field propagate — GIGO.
- **Zhang & Wang 2006** (*JTECH* 23, 1239–1248, doi:10.1175/JTECH1910.1).
  2D multipass: unfold only high-confidence discontinuities on early
  passes; defer low-confidence gates to later passes with more context. No
  external reference needed. Lesson kept: **defer, don't guess** —
  v4's repair gauntlet orders modules from strict to relaxed.
- **Louf et al. 2020, UNRAVEL** (*JTECH* 37, 741–758,
  doi:10.1175/JTECH-D-19-0020.1; reference implementation
  github.com/vlouf/dealias). Modular iterative gauntlet run to convergence:
  find least-aliased reference radials → initialize → iterated
  range/azimuth continuity passes with growing windows (6, 12 … radials) →
  box checks over growing (az×range) windows (5×2, 20×10, 40×20) →
  least-squares / linear-regression checks → closest-point module → 3D
  continuity against the sweep below → final least-squares and box
  validation, with completion checks between stages. Two strategies
  (default / long-range) select window ladders. Lesson kept: a
  **post-solve module gauntlet with convergence checks** catches the long
  tail; each module must be individually safe.
- **Feldmann et al. 2020, R2D2** (*JTECH* 37, 2341–2356,
  doi:10.1175/JTECH-D-20-0054.1). Region-based recursive dealiasing that
  **masks high-shear areas (with a buffer) before segmentation** so
  mesocyclone couplets neither break segmentation nor get smoothed.
  Lesson kept: explicit couplet protection (F11).
- **Helmus & Collis 2016, Py-ART** (*JORS* 4(1):e25, doi:10.5334/jors.119).
  `dealias_region_based`: region segmentation via velocity-interval
  splitting + edge-weighted region combination. The interval-splitting idea
  was tested here against F2 and was insufficient alone (the bad branch
  survives the vote graph) — v4 keeps splitting as segmentation hygiene but
  does not rely on it for branch truth.
- **He, Sun & Ying 2019, ADTH** (*JTECH* 36, 139–149,
  doi:10.1175/JTECH-D-17-0165.1). Typhoon/hurricane-specialized dealiasing:
  vortex-aware references because axisymmetric storm flow breaks
  uniform-wind assumptions. Lesson kept: our harmonic fit is per range band
  (not one global VAD), and the eval battery includes a TC case so we
  measure rather than assume.
- **Veillette, Kurdzo et al. 2023** (*AIES* 2(3), e220084,
  doi:10.1175/AIES-D-22-0084.1; arXiv:2211.13181). U-Net emulation of the
  ORPG 2D VDA, trained on ORPG output as truth; accurate, fast, portable.
  Considered and **rejected** for v4 core (§4e).
- **Commercial (GR2Analyst-class).** Public knowledge: the GRLevelX manual
  documents a dealiasing settings dialog with a *radial continuity
  threshold* and an *azimuth continuity factor* (multiples of Nyquist) and
  optional ND substitution when a gate cannot be reliably dealiased
  (grlevelx.com manuals; storm motion entry is documented for the SRV
  display). The often-repeated "storm-motion-referenced two-pass
  dealiasing" description of GR2Analyst is **community inference, not
  vendor documentation** — we note it and do not design against it.
  RadarScope's dealiaser is likewise undocumented. What we CAN say
  honestly: no desktop competitor documents an environmental-model absolute
  reference, a volume-joint branch solve, or a confidence output; ORPG
  (the operational reference implementation) uses environmental winds but
  runs server-side.

---

## 4. v4 design: pillars, with adopt/adapt/reject decisions

v4 = the region core (kept) + four pillars. One volume-level solve, per-tilt
outputs, everything deterministic.

### 4a. Environmental absolute reference — **ADOPT**

The single change that decides F1/F3/F4/F5/F7: an **external, non-circular
branch anchor** (4DD's sounding initialization, James & Houze 2001; ORPG's
environmental winds precedent, Eilts & Smith 1990). Constraint honored:
render2d cannot depend on app model-data plumbing, so the profile is a
caller-supplied value object and `Option`:

```rust
/// Environmental wind profile at the radar site, supplied by the caller
/// (the app will wire HRRR/RAP analysis winds later; tests use fixtures).
/// INVARIANT: render2d never fetches anything. With `None`, v4 degrades
/// gracefully to v3-hybrid-class evidence (temporal + vertical + harmonic).
pub struct EnvironmentalWindProfile {
    /// Levels strictly increasing in height above radar level (meters).
    pub levels: Vec<EnvWindLevel>,
    /// Model valid time. The engine ignores profiles more than
    /// ENV_PROFILE_MAX_AGE (3 h) from the volume time — a stale profile is
    /// worse than none in evolving synoptic flow.
    pub valid_time: chrono::DateTime<chrono::Utc>,
}

pub struct EnvWindLevel {
    pub height_m_arl: f32,
    pub u_mps: f32, // eastward
    pub v_mps: f32, // northward
}
```

Projection to a predicted radial velocity per gate: beam height from the
4/3-effective-earth-radius model (Doviak & Zrnić 1993, *Doppler Radar and
Weather Observations*, 2nd ed., §2.2), linear interpolation of (u, v) in
height, then v̂ᵣ = (u·sin az + v·cos az)·cos φₑ (elevation φₑ; vertical air
motion deliberately ignored — documented invariant: the profile is a
**branch separator**, not a truth field; branch spacing 2N ≥ ~23 m/s vs
HRRR/RAP low-level RMS vector error of ~3–5 m/s leaves margin).

Why this is safe where the VAD was not (F4): the model profile cannot be
poisoned by the sweep's own wrapped gates. Why it must not dominate: real
storms (RIJ, meso, TC eyewall) deviate hugely from the environmental wind —
so it enters the energy (§5) as a **robust, capped, low-weight unary term**
that decides branches only when storm-scale evidence is silent or split.
The KMBX blob is exactly that situation.

### 4b. Joint whole-volume branch optimization — **ADAPT (scoped)**

Adopted: all velocity tilts' regions + temporal prior + environmental prior
in **one global discrete assignment problem** (fixes F3, F5, F12; the
Jing & Wiener lesson lifted from sweep scope to volume scope). Scoped-down
from a free-form MRF: labels are per-**super-region** branch shifts
k ∈ {−2…+2}, not per-gate states, so the graph is tiny (tens of nodes) and
the solver can be boring (§5). Rejected variants: per-gate MRF via graph
cuts (Boykov, Veksler & Zabih 2001, *IEEE TPAMI* 23, 1222–1239) — a heavy
dependency and unnecessary at region granularity; ILP with an external
solver — dependency and determinism risk.

### 4c. UNRAVEL-style iterative repair gauntlet — **ADOPT (with guards)**

Post-solve per-tilt modules (Louf et al. 2020 pattern; Zhang & Wang 2006
defer-don't-guess ordering) run to convergence with strict do-no-harm rules
(§7). This is what closes hybrid's patch rings (F8/§1.4 finding 2) and the
long tail of specks — and where couplet protection (R2D2, F11) is enforced.

### 4d. Quality/confidence output — **ADOPT**

Per-region branch-decision margins are already computed by the solver; we
keep them (§8) as a per-gate `u8` grid. Two consumers now, zero UI work:
(1) **temporal confidence propagation** — the next volume weights its
temporal prior by the previous solution's confidence, which is the designed
break for the F6 GIGO loop (KMBX 23:40's blob would carry a low margin, so
23:54's temporal weight drops and the environmental term decides);
(2) the eval harness reports confidence-weighted error. UI display is
explicitly out of scope.

### 4e. ML dealiasing — **REJECT for v4 core**

Veillette et al. 2023 is credible (U-Net matches ORPG 2D VDA skill, runs
fast). Rejected for a shipped Rust desktop engine because: (1) its truth
source is itself an algorithm (ORPG output), capping skill at "emulation of
the thing we are trying to beat"; (2) weights + an ONNX/tract runtime add
tens of MB and a heavy dependency to render2d, which currently has none of
that kind; (3) bit-determinism across CPU/GPU backends is not guaranteed,
and determinism is a project invariant (the bench fails on checksum drift);
(4) failure modes are unbounded and undebuggable for the weaker-model
maintainers this codebase is being written for (decision 12b); (5) physical
guarantees (whole-Nyquist-multiple moves only) are structural in v4 but
would need to be bolted onto a network's output. Revisit post-v4, if ever,
as an offline confidence assist — not as the dealiaser.

### 4f. Orchestrator-flagged candidates not listed above

- **Storm-motion-referenced two-pass (GR2Analyst folklore)** — REJECT as a
  distinct mechanism: storm motion is a display-frame concept; the
  temporal prior already encodes storm-following continuity with actual
  data, and the environmental profile is a strictly stronger absolute
  anchor. Nothing additional to adopt.
- **Velocity-interval region splitting (Py-ART)** — ADOPT as segmentation
  hygiene inside super-region construction (§5.1), with the documented
  caveat that it does not by itself fix F2/F3 (tested on the real volume).

---

## 5. Exact formulation

### 5.1 Super-regions

Per velocity tilt, run the existing v1 segmentation (flood fill +
inter-region fold votes + `FoldUnionFind`), unchanged — its behavior is
pinned by the existing test suite. Then classify each resolved vote edge:

- **strong**: support ≥ `STRONG_EDGE_MIN_SUPPORT` (12 boundary pairs) AND
  winning-fold vote share ≥ `STRONG_EDGE_MIN_SHARE` (0.80);
- **weak**: everything else (including every edge that F3's post-mortem
  showed can silently misbranch a subgraph).

A **super-region** is a connected component of regions over strong edges
only, carrying the v1-relative fold offsets `off_r` within it. Weak edges
become soft pairwise terms below instead of hard unions. This is the direct
structural answer to F3: a single spurious contact can no longer weld two
internally-consistent subgraphs into one rigid body.

Nodes smaller than `SUPERREGION_MIN_GATES` (64) skip the graph: they keep
their local solve and are handled by the repair gauntlet (they have too few
gates to accumulate meaningful unary evidence).

### 5.2 Labels and energy

Unknowns: one integer label kₛ ∈ {−2, …, +2} per super-region s (branch
shift on top of its local anchor; the final per-gate fold stays clamped to
±`REGION_MAX_FOLD` = 5). With G = all finite gates of the volume, evidence
tables are built in **one O(G) pass per evidence kind** (same order as one
v1 pass over all tilts, ≈176 ms measured; §10.5 budgets it).

Energy to minimize, all costs in m/s so weights are dimensionless:

```
E(k) = Σ_s [ λ_env · w(cov_env(s))  · C_env(s, k_s)
           + λ_tmp · conf_prev(s) · w(cov_tmp(s)) · C_tmp(s, k_s) ]
     + Σ_(s,t) vertical  λ_vrt · w(cov_vrt(s,t)) · C_vrt(s, t, k_s, k_t)
     + Σ_(s,t) weak-edge λ_wk  · share(e) · min(|Δk_e|, 2)
```

- `C_env(s, k)` — mean over env-covered gates of
  min(|v + (off + k)·2N − v̂_env|, LOSS_CAP) with `LOSS_CAP` = 40 m/s (the
  existing `DENSE_REFERENCE_LOSS_CAP_MPS`; robust so storm-scale deviations
  from the environment can't buy a wrong branch).
- `C_tmp(s, k)` — same form against the previous solution mapped onto this
  tilt's lattice (reuse `hybrid.rs` mapping); `conf_prev(s)` = mean previous
  confidence (§8) over the mapped gates, in [0, 1]. **This term is a unary,
  not a pairwise: the previous volume is solved and fixed.**
- `C_vrt(s, t, k_s, k_t)` — mean capped |unfolded_s − unfolded_t| over
  co-located (az, range) samples between vertically adjacent tilts
  (elevation gap ≤ 3.0°, the existing
  `SPATIAL_REFERENCE_MAX_ELEVATION_DELTA_DEG`), evaluated for all 25 label
  pairs. Nearest-neighbor lattice mapping as in `hybrid.rs`. SAILS
  revisits of the same elevation get temporal-style edges to their sibling
  cut instead of vertical ones (they are separate times, not heights —
  same rule the hybrid already applies).
- weak intra-tilt edges: `Δk_e = (off_s + k_s) − (off_t + k_t) − f_e` where
  `f_e` is the edge's majority fold; `share(e)` = winning vote share ∈
  (0, 1]. Cost 0 when the labels honor the observed boundary fold.
- `w(cov) = cov / (cov + 64)` — one smooth saturation so 20 covered gates
  don't outvote 20,000 (explicit, no magic curve).
- Anchor: with an environmental profile, the absolute branch is decided by
  the energy itself (that is the point). Without one, add a tie-break unary
  `λ_anchor·|k_s|` (λ_anchor = 0.01) so the solve reproduces v1's
  "largest-region fold 0" behavior when no external evidence exists —
  graceful degradation, pinned by test.

Provisional weights (Stage 5 calibrates on the battery, §11): λ_vrt = 1.0,
λ_tmp = 0.8 (×conf), λ_env = 0.6, λ_wk = 0.4. Rationale: current-time
observation > continuity from t−1 > model prior > ambiguous boundary — and
the env term still wins when everything else is silent (KMBX: cov_tmp low
confidence, vertical tilts equally folded ⇒ their C_vrt is
branch-degenerate: shifting both tilts together costs nothing, so the env
unary is the only symmetry breaker. This degeneracy is WHY v2/v3 fail F5
and why λ_env need not be large).

### 5.3 Solver

Graph size on real volumes: tens of nodes (KEAX ≈ 17 velocity cuts × a few
super-regions each), labels 5. Boring, deterministic, exact-where-cheap:

1. **Spanning-forest DP (exact on the forest).** Maximum spanning forest
   over pairwise-evidence weight (Kruskal, edges pre-sorted by
   (weight desc, tilt, region id) for determinism). Tree DP over labels:
   O(nodes · 5²) — microseconds.
2. **ICM refinement** over the full graph including non-forest edges
   (Besag 1986, *JRSS-B* 48, 259–302): sweep nodes in fixed (tilt, region)
   order, re-optimizing each label given neighbors; repeat to fixed point,
   max 10 sweeps. Deterministic; monotonically non-increasing energy.
3. **Exhaustive verification** for any connected component with ≤ 8 nodes:
   enumerate 5⁸ = 390,625 assignments against precomputed tables
   (each evaluation is table lookups; < 10 ms worst case). If enumeration
   beats DP+ICM, take it — and count it in diagnostics (a nonzero count
   means the heuristic missed; watch it in the battery).

Justification of tractability: per-region branch count is small (5) and
super-region count is small because strong-edge merging keeps components
coarse; the expensive part is table construction, which is linear in gates
and shared across all tilts once per volume — that is the F12 fix.

### 5.4 Determinism invariant

Same input volume (+ same priors) ⇒ byte-identical output grids and
confidence, across runs and thread counts. All reductions over gates use
fixed iteration order or order-independent accumulators (f64 sums per
row-chunk merged in row order); all sorts use `total_cmp` with full tie
chains. Pinned by test (§11 Stage 3) — the dealiaser nondeterminism class
of bug has bitten this codebase before.

---

## 6. Module layout and pub surface

Per 12b: several small files, one concern each, `pub(crate)` internals,
`pub` only at the entry module. New directory `crates/render2d/src/dealias_v4/`:

| file | concern | est. size |
|---|---|---|
| `mod.rs` | pub surface, top-level doc (invariants, citations), orchestration | ~200 |
| `env_profile.rs` | `EnvironmentalWindProfile`, beam height, v̂ᵣ projection, staleness | ~250 |
| `super_regions.rs` | strong/weak edge split, `SuperRegion` build per tilt | ~300 |
| `graph.rs` | evidence tables: env/temporal unaries, vertical + weak pairwise | ~350 |
| `solve.rs` | forest DP + ICM + small-component enumeration | ~300 |
| `repair.rs` | gauntlet modules + do-no-harm ledger (§7) | ~350 |
| `confidence.rs` | margins → per-gate u8; propagation helpers (§8) | ~150 |

Prerequisite refactor (Stage 2): the v1 internals v4 needs (`region_of`,
`region_size`, resolved vote edges, `FoldUnionFind` offsets) move from the
5,622-line `lib.rs` into `crates/render2d/src/region_core.rs` with **zero
behavior change**, re-exported so `dealias_velocity_grid*` signatures and
all existing tests stand unmodified. That extraction is a 12b deliverable
in its own right.

Public surface (mirrors the hybrid's calling convention, plus the
volume-level entry):

```rust
/// Solve the whole volume once. `previous` enables the temporal prior;
/// prefer passing the previous *solution* so confidence propagates (F6);
/// a bare previous volume is accepted and solved v1-style as fallback.
pub fn dealias_volume_v4(
    volume: &RadarVolume,
    previous: Option<TemporalPrior<'_>>,
    environment: Option<&EnvironmentalWindProfile>,
) -> V4VolumeSolution;

pub enum TemporalPrior<'a> {
    Solution(&'a V4VolumeSolution),
    Volume(&'a RadarVolume),
}

impl V4VolumeSolution {
    /// Dealiased grid for a cut, if it carried velocity. O(1); grids are
    /// materialized by the solve.
    pub fn tilt_grid(&self, cut_index: usize) -> Option<&MomentGrid>;
    pub fn tilt_confidence(&self, cut_index: usize) -> Option<&ConfidenceGrid>;
    pub fn diagnostics(&self) -> &V4Diagnostics; // per-module change counts, energy, enum-vs-heuristic disagreements
}

/// Drop-in per-cut convenience mirroring `dealias_velocity_grid_hybrid`.
/// Runs the volume solve internally — callers that need >1 tilt should
/// hold a `V4VolumeSolution` instead (documented cost note).
pub fn dealias_velocity_grid_v4(
    volume: &RadarVolume,
    cut_index: usize,
    previous: Option<&RadarVolume>,
    environment: Option<&EnvironmentalWindProfile>,
) -> Option<MomentGrid>;
```

The app's render worker caches per (volume, product); `V4VolumeSolution` is
the natural cache value (one solve per volume, all velocity tilts served
from it). Wiring is the app fleet's job, not this spec's.

---

## 7. Repair gauntlet (post-solve, per tilt)

Modules in order, strict → relaxed (Zhang & Wang 2006 ordering; window
ladder after UNRAVEL, Louf et al. 2020). All modules obey the **global
rules**: (a) moves are whole ±2N multiples only; (b) a gate inside the
shear-couplet mask is never modified; (c) a module that wants to touch
> `REPAIR_MAX_FRACTION` (0.15) of the tilt's finite gates aborts, changes
nothing, and sets a diagnostic flag (do-no-harm); (d) every applied change
must reduce the local objective it tested — no speculative flips.

- **R0 couplet mask** (Feldmann et al. 2020 R2D2). Mark gates in compact
  azimuthal couplets: opposite-sign neighbors within ≤ 5 gates azimuthally
  whose |Δv| ≥ 1.4·N, dilated by 2 gates. These are candidate mesocyclones
  /TVSs; the mask exempts them from R2–R3 smoothing-style checks (F11).
- **R1 speck snap.** The existing 4-neighbor median despeckle
  (Holleman & Beekhuis 2003; Altube et al. 2017), unchanged.
- **R2 coherent-patch branch repair.** Generalization of
  `refine_local_branch_patches`: per-gate branch proposal against the best
  covering reference (priority: temporal·conf, vertical, env), 3-of-5
  neighbor coherence, margins scaled to min(8 m/s, 0.3·2N) so the pass
  stays live in low-Nyquist regimes (F10). **New:** after applying a patch,
  its boundary ring is re-tested — ring gates whose |Δv| to the patch
  interior is < 0.5·2N but whose |Δv| to the exterior is > 1.2·2N join the
  patch (this is the specific fix for §1.4 finding 2).
- **R3 box-median check.** For each gate, median of finite gates in
  (az × range) windows 5×2, then 20×10, then 40×20 (UNRAVEL's ladder): if
  |v − med| > 1.2·N and one whole-2N move lands within 0.4·2N of the
  median, move it. Iterate the ladder to convergence, max 3 rounds.
- **R4 least-squares plane check.** Fit v(az, range) as a plane over each
  40×20 window (UNRAVEL's least-squares module); gates > 1.2·N off the
  plane get the R3 test against the plane value. One pass after R3
  converges; skipped entirely if R3 hit its change cap.
- **Convergence:** repeat R3–R4 until a full round changes zero gates
  (max 3 rounds). Every module logs gates-changed to `V4Diagnostics`;
  the battery (§10) regression-gates those counts.

---

## 8. Confidence output

`ConfidenceGrid` = `Vec<u8>` parallel to the moment grid, 0 = no data /
no opinion, 255 = decisive.

- Per super-region: margin m_s = (E(second-best kₛ) − E(best kₛ)) computed
  with all other labels fixed at the optimum, normalized per covered gate;
  mapped 0–12 m/s → 0–255, saturating (12 m/s spans the existing
  `REFERENCE_AGREEMENT_MPS`, i.e. "clearly separated branches").
- Regions below `SUPERREGION_MIN_GATES` and graph-degenerate groups (no
  external coverage at all): fixed 64 ("interior-consistent only").
- Gates changed by R2–R4: `min(existing, 96)`. Gates snapped by R1: 32.
- Consumed by: temporal propagation (§5.2 `conf_prev`), the eval harness,
  and — someday, not now — the UI. Cost: one u8 per gate (≈0.9 MB per
  super-res tilt), computed from numbers the solver already has.

---

## 9. Failure-mode handling map

| mode | v4 mechanism | residual risk |
|---|---|---|
| F1 group branch | env unary (4a) + vertical/temporal in one energy | no env profile on cold start ⇒ back to v3-class behavior, flagged low confidence |
| F2 mega-region | interval splitting hygiene + weak-edge softening lets the solver split branch decisions | pathological gradual ramps may still chain; battery watches KEAX |
| F3 subgraph misbranch | weak edges are soft terms, not hard unions; env/vertical unaries pull subgraphs independently | a subgraph welded by a *strong* (high-support) spurious edge stays welded; threshold calibrated at Stage 5 |
| F4 VAD circularity | VAD demoted to tie-break; env profile is non-circular | none beyond profile availability |
| F5 all-tilts-aliased | vertical terms are branch-degenerate but harmless; env decides | KMBX 235423 is the acceptance test |
| F6 temporal GIGO | confidence-weighted temporal unary | first bad volume w/ high confidence can still propagate once; margin normalization keeps that rare |
| F7 cold start | env-only anchoring works with zero history | no env + cold start = documented degradation |
| F8 fused pocket | R2 with ring-closure | pockets smaller than coherence quorum stay |
| F9 isolated cells | env unary covers every gate with echo | env error > 0.5·2N at very low Nyquist could misbranch an isolated cell; capped loss + confidence floor mitigate |
| F10 low Nyquist | margins scale with N; dual-PRF despeckle kept; Nyquist-`None` pass-through kept | genuinely unrecoverable when 2N ≈ profile error; confidence says so |
| F11 couplet smoothing | R0 mask exempts couplets from repair | mask FPs reduce repair coverage locally (accepted trade) |
| F12 per-tilt inconsistency | single volume solve, vertical pairwise terms | none |

---

## 10. Eval battery ("best in existence" is a measured claim)

### 10.1 Cases (all volumes verified present on
`https://unidata-nexrad-level2.s3.amazonaws.com/YYYY/MM/DD/SSSS/<id>`; the
bench README's download pattern applies; 2013 keys carry `.gz`)

| case | volumes (target + temporal prior) | regime |
|---|---|---|
| A derecho | `KEAX20260609_055143_V06` (prior `KEAX20260609_054454_V06`) | QLCS/RIJ, widespread aliasing, N = 24.1; the canonical acceptance case |
| B tornadic supercell | `KTLX20130520_201643_V06` (prior `KTLX20130520_201229_V06`) | Moore EF5 mesocyclone at ~20–35 km; couplet preservation; N = 26.1 |
| C hurricane | `KLIX20210829_163252_V06` (prior `KLIX20210829_162629_V06`) | Ida eyewall approaching landfall; multi-fold (|fold| ≥ 2), split cuts N = 23.2/32.1 |
| D QLCS misbranch pair | `KMBX20260609_235423_V06` (prior `KMBX20260609_234726_V06`); positive control `KMBX20260609_234055_V06` (prior `KMBX20260609_233434_V06`) | F3/F5/F6 regression set from `dealias-fold-branch-analysis.md` |
| E low-Nyquist rewrap | synthetic: lowest tilts of A and B, truth = accepted native-Nyquist v4 output, re-wrapped to N = 12 m/s, presented as single-tilt volumes with no priors | F7+F10: cold start at clear-air-class Nyquist, exact ground truth |

Case E is synthetic **on purpose**: TDWR Level-II base data has no public
archive, the EUMETNET ORD bucket retains only 24 h (not reproducible), and
VCP-31 volumes cannot be identified in the archive without decoding
headers. Rewrap gives exact truth, the thing real cases never do. A real
archived VCP-31 LLJ volume is Open Question 1.

Environmental fixtures: per case, a checked-in JSON `EnvironmentalWindProfile`
hand-extracted once from the archived RAP/RUC 0-h analysis at the radar
site and volume time (source noted in the fixture header). Cases must also
run **without** the fixture to measure graceful degradation.

### 10.2 Metrics (exact definitions)

1. **Residual fold-boundary pairs** — adjacent finite 4-neighbor pairs
   (including the azimuth wrap seam) with |Δv| > 1.2·N_row; reported for
   the lowest super-res tilt and volume-total. *Never read alone* (§1.4
   finding 2: a consistently-wrong field scores ~0).
2. **Reference RMS** — RMS of (v − v̂_env) over env-covered gates, plus the
   same against the Browning & Wexler per-band fit. A prior, not truth:
   used as a cross-engine delta, gated loosely.
3. **% gates branch-modified** (fold ≠ 0 vs raw) — reported; large deltas
   vs hybrid must be explained in the acceptance notes.
4. **Isolated speck count** — 4-connected components of ≤ 3 gates whose
   mean |v − 8-neighborhood median| > N.
5. **Branch spot-checks** (5×5-gate means):
   - D: `235423` az 339°/20 km must be **inbound** (raw +13.3 → ≈ −39);
     az 316°/21 km inbound; `234055` az 202°/57.9 km ≈ −33 (hybrid parity —
     both engines already get −34.0).
   - B: strongest azimuthal couplet on the lowest tilt, 15–40 km: v4
     couplet ΔV within −2 m/s of hybrid's (repair must not smooth it).
   - C: max inbound in the eyewall annulus ≥ hybrid's (no under-unfold);
     |fold| ≥ 2 gates must form coherent regions (each ≥ 32 gates), not
     speckle.
   - E: % gates on the correct branch (|v − truth| < 6 m/s). Target ≥ 99%
     with the env fixture; hybrid baseline recorded at Stage 0 for the
     material-win comparison.
6. **Runtime** — per-tilt worst super-res and whole-volume, release,
   best-of-5, same machine as §1.4.
7. **Determinism** — two consecutive runs byte-identical (grids +
   confidence), enforced by the harness exit code like `bowecho-bench`.

### 10.3 Pass criteria

Per case, v4 must **beat or tie the hybrid on every metric** (ties within
noise: boundaries ±5%, RMS +0.5 m/s, specks ±5, runtime within budget),
and must **beat it materially on at least two cases**, where material =
≥ 30% fewer residual boundaries at equal-or-better reference RMS, or
flipping a failed spot-check to pass. Two material wins are already
identified and required:

- **D is a hard requirement**: the hybrid demonstrably fails the 235423
  blob today (§1.4); v4 with the env fixture must pass all three D
  spot-checks and the `dealias-fold-branch-analysis.md` validation list
  (blob inbound; 234055 az 202 ≈ −33; KEAX ≤ ~250 boundaries; unit suite
  incl. `region_dealias_recovers_smooth_folded_ramp`).
- **E is a hard requirement**: ≥ 99% correct-branch with env fixture;
  hybrid (no env input exists for it) is expected to be far below —
  recorded, not assumed.

Additionally v4-without-env must be ≥ hybrid on A–D (graceful
degradation), and v4's fold-boundary count must also be ≤ the **region**
engine's on A (the hybrid's 527 vs 264 regression must not be inherited).

### 10.4 Baseline discipline

Stage 0 (§11) lands the harness (bench extension + fixtures), re-measures
region/hybrid on A–D with the exact §10.2 metric code, and pins the numbers
in `docs/dealias-v4-baselines.json` (or a table in this file — implementer's
choice, but checked in). The §1.4 numbers are provisional until then; the
historical "21,458 → 196" figure used a slightly different boundary metric
and is reconciled at Stage 0 (the scratch harness reproduces 264 at the
1.2·N threshold on the same volume — same field, different ruler).

### 10.5 Runtime budget

Measured hybrid ballpark (§1.4): 76–146 ms worst super-res tilt; 955 ms
KEAX whole volume when every tilt pays independently. Budget (same machine
class):

- **volume solve ≤ 650 ms** for a KEAX-class VCP 212 volume with SAILS
  (17 velocity cuts) — beats hybrid-everywhere while doing strictly more;
- **worst single-tilt amortized cost ≤ 150 ms** (solve ÷ tilts served, and
  first-tilt latency: partial-volume solve of the lowest tilt with only its
  vertical neighbor must stay ≤ 150 ms so live-feed first paint doesn't
  regress);
- `tilt_grid()` after solve: O(1), no budget needed;
- evidence-table pass target ≤ 250 ms of the 650 (it is one O(G) sweep,
  and v1 already does an O(G) sweep in 176 ms).

---

## 11. Staged build order (each stage lands green with its own tests)

- **Stage 0 — harness + baselines.** Extend `crates/bench` with a
  `--dealias` mode (per-engine, per-tilt timings; §10.2 metrics; JSON out;
  nonzero exit on nondeterminism, same discipline as the pixel checksum).
  Case fixtures: download script/README (bench itself never fetches — keep
  that invariant), env-profile JSONs. Record region + hybrid baselines on
  A–D. Tests: metric unit tests on synthetic grids (a known 2-fold ramp
  yields exact counts); harness determinism.
- **Stage 1 — `env_profile.rs`.** Struct, beam-height model, projection,
  staleness guard. Tests: uniform wind recovers v̂ᵣ = speed·cos(az−dir) at
  0° elevation; interpolation between levels; 4/3-earth height matches
  Doviak & Zrnić table values at 100/200 km; stale profile ⇒ `None`
  behavior. Cite Doviak & Zrnić 1993; James & Houze 2001 in the module doc.
- **Stage 2 — `region_core.rs` extraction.** Move v1 internals out of
  `lib.rs` unchanged; re-export; add one golden test (KEAX tilt fixture →
  identical fold array hash before/after). No new behavior. This is the
  riskiest refactor, so it ships alone.
- **Stage 3 — `super_regions.rs` + `graph.rs` + `solve.rs`.** Volume graph
  and solver behind a feature-gate entry (not yet public). Tests:
  synthetic F3 reproduction (internally-consistent subgraph attached by one
  weak edge + env prior ⇒ subgraph rebranched; without env ⇒ v1-identical
  output, pinning graceful degradation); branch-degenerate two-tilt case
  (F5) decided by env; determinism (two runs byte-identical, and identical
  under `RAYON_NUM_THREADS=1` vs default); enumeration-vs-heuristic
  agreement on all synthetic cases.
- **Stage 4 — `repair.rs`.** Gauntlet + guards. Tests: synthetic meso
  couplet survives R0–R4 untouched (F11); hybrid's
  `local_refinement_repairs_a_folded_patch…` scenario passes with **zero
  residual ring pairs** (the §1.4 finding-2 pin); change-cap abort works;
  R3 ladder converges ≤ 3 rounds on a speckled fixture.
- **Stage 5 — `confidence.rs` + `mod.rs` pub surface + calibration.**
  Temporal propagation wired (`TemporalPrior::Solution`). Run the full
  battery; calibrate λ/threshold constants; freeze them as named consts
  with rationale comments. Tests: the hybrid test-suite scenarios ported to
  v4 (all four must pass); D-pair integration test on real volumes (marked
  `#[ignore]`, run by the battery like bench's smoke test).
- **Stage 6 — acceptance.** Full battery vs Stage-0 baselines; results
  table appended to this spec; bench budget gate green; only then does the
  app fleet get the go-ahead to wire `EnvironmentalWindProfile` and swap
  the default engine.

Rollback stance: v1/v2/v3 remain in the tree untouched; v4 is additive and
selectable, so a battery failure blocks promotion, not the codebase.

---

## 12. Open questions (for the owner / orchestrator)

1. **Real low-Nyquist archive volume**: worth a one-time scan of candidate
   overnight-LLJ dates (decode just the VCP header of ~50 candidate
   volumes from northern-plains sites) to find a genuine VCP-31 case to
   sit beside synthetic Case E? (~1 h of work, better external validity.)
2. **Profile source policy** (app side, out of render2d scope): HRRR vs
   RAP, analysis vs short forecast, fetch cadence, and offline behavior.
   The interface freezes regardless; only fixture provenance changes.
3. **Solution retention**: should the app cache retain `V4VolumeSolution`
   per volume for temporal propagation (≈ one grid + 1 B/gate × velocity
   cuts ≈ 25–30 MB per super-res volume)? If not, `TemporalPrior::Volume`
   fallback costs a re-solve and loses confidence weighting.
4. **Calibration overfit risk**: λ/thresholds are tuned on A–E (n=5, one
   KMBX event for F3/F5). Do we want 2–3 more QLCS misbranch cases mined
   from 2026 archives before freezing constants?
5. **Historical metric reconciliation**: confirm at Stage 0 which boundary
   threshold produced 21,458 → 196 so the acceptance line "KEAX ≤ ~250"
   stays apples-to-apples.
6. **SAILS revisit handling** in the graph (temporal-style edges, §5.2) —
   sanity-check against a MESO-SAILS×3 volume at Stage 3; if revisit
   mapping proves noisy, drop those edges (they are additive, not
   load-bearing).

---

## 13. References

- Altube, P., J. Bech, O. Argemí, T. Rigo, N. Pineda, S. Collis, and
  J. Helmus, 2017: Correction of dual-PRF Doppler velocity outliers in the
  presence of aliasing. *J. Atmos. Oceanic Technol.*, 34, 1529–1543.
- Besag, J., 1986: On the statistical analysis of dirty pictures.
  *J. Roy. Stat. Soc. B*, 48, 259–302.
- Boykov, Y., O. Veksler, and R. Zabih, 2001: Fast approximate energy
  minimization via graph cuts. *IEEE TPAMI*, 23, 1222–1239.
- Browning, K. A., and R. Wexler, 1968: The determination of kinematic
  properties of a wind field using Doppler radar. *J. Appl. Meteor.*, 7,
  105–113.
- Doviak, R. J., and D. S. Zrnić, 1993: *Doppler Radar and Weather
  Observations*, 2nd ed., Academic Press (beam-height 4/3-earth model,
  §2.2).
- Eilts, M. D., and S. D. Smith, 1990: Efficient dealiasing of Doppler
  velocities using local environment constraints. *J. Atmos. Oceanic
  Technol.*, 7, 118–128.
- Feldmann, M., C. N. James, M. Boscacci, D. Leuenberger, M. Gabella,
  U. Germann, D. Wolfensberger, and A. Berne, 2020: R2D2: A region-based
  recursive Doppler dealiasing algorithm for operational weather radar.
  *J. Atmos. Oceanic Technol.*, 37, 2341–2356, doi:10.1175/JTECH-D-20-0054.1.
- He, G., J. Sun, and Z. Ying, 2019: An automated velocity dealiasing
  scheme for radar data observed from typhoons and hurricanes. *J. Atmos.
  Oceanic Technol.*, 36, 139–149, doi:10.1175/JTECH-D-17-0165.1.
- Helmus, J. J., and S. M. Collis, 2016: The Python ARM Radar Toolkit
  (Py-ART). *J. Open Res. Softw.*, 4(1), e25, doi:10.5334/jors.119.
- Holleman, I., and H. Beekhuis, 2003: Analysis and correction of dual-PRF
  velocity data. *J. Atmos. Oceanic Technol.*, 20, 443–453.
- James, C. N., and R. A. Houze Jr., 2001: A real-time four-dimensional
  Doppler dealiasing scheme. *J. Atmos. Oceanic Technol.*, 18, 1674–1683.
- Jing, Z., and G. Wiener, 1993: Two-dimensional dealiasing of Doppler
  velocities. *J. Atmos. Oceanic Technol.*, 10, 798–808.
- Louf, V., A. Protat, R. C. Jackson, S. M. Collis, and J. Helmus, 2020:
  UNRAVEL: A robust modular velocity dealiasing technique for Doppler
  radar. *J. Atmos. Oceanic Technol.*, 37, 741–758,
  doi:10.1175/JTECH-D-19-0020.1 (reference impl. github.com/vlouf/dealias).
- Veillette, M. S., J. M. Kurdzo, P. M. Stepanian, T. Meuse, J. McDonald,
  and S. Samsi, 2023: A deep learning–based velocity dealiasing algorithm
  derived from the WSR-88D Open Radar Product Generator. *Artif. Intell.
  Earth Syst.*, 2(3), e220084, doi:10.1175/AIES-D-22-0084.1.
- Zhang, J., and S. Wang, 2006: An automated 2D multipass Doppler radar
  velocity dealiasing scheme. *J. Atmos. Oceanic Technol.*, 23, 1239–1248,
  doi:10.1175/JTECH1910.1.
- GRLevelX GR2Analyst manual (grlevelx.com/manuals/gr2analyst/) — public
  documentation of the dealiasing settings (radial continuity threshold,
  azimuth continuity factor, ND substitution); anything beyond that about
  commercial dealiasers is marked as inference in §3.

---

## 14. Stage-6 battery results (2026-07-02, implementation as landed)

Full numbers pinned in `docs/dealias-v4-baselines.json`; harness
`bowecho-bench --dealias` (best-of-3, Ryzen 9 9950X3D, `--release`).
Metric key: bnd = residual fold-boundary pairs (lowest tilt / volume
total), rmsE = RMS vs env fixture on the lowest tilt.

| case | region | cascade | hybrid | v4 (env) | v4-noenv |
|---|---|---|---|---|---|
| A KEAX bnd low/vol | **264** / 2869 | 270 / 2893 | 527 / 7064 | **257 / 5059** | 405 / 11189 |
| B KTLX bnd; couplet ΔV | 84 / 1706; 103.5 | 98; 56.2 | 127 / 4601; **56.9 (smoothed)** | **96 / 2986; 120.7 (preserved)** | 205; 120.7 |
| C KLIX bnd low/vol | 298 / **1047** | 602 / 3024 | 1288 / 11174 | **522** / 19715 | 428 / 19658 |
| D KMBX bnd low/vol | 137 / 1176 | 150 / 1977 | 164 / 11620 | **140 / 2314** | 3808 / 26178 |
| D blob az339/20 km (truth ≈ −39) | +13.3 | +13.3 | +13.3 | **+13.3 (unfixed)** | +13.3 |
| D-control az202/57.9 (truth ≈ −33) | −34.0 ✓ | −34.0 ✓ | −34.0 ✓ | **−34.0 ✓** | −34.0 ✓ |
| E KEAX N=12 correct-branch % | 74.08 | 74.08 | 74.08 | 82.73 | **95.25** |
| E KTLX N=12 correct-branch % | 77.45 | 77.45 | 77.45 | **93.52** | 77.01 |
| runtime, KEAX volume / worst tilt | 158 ms / 19 ms | 1067 / 162 | 943 / 124 | **540 / 32 (amortized)** | 497 / 29 |

Determinism: every engine byte-identical across runs on every case
(harness exit-code gate).

**Verdict — met:**
- A: v4 beats REGION (257 < 264) and hybrid (−51% boundaries, fewer
  specks, equal RMS) — material win; the §10.3 "must not inherit
  hybrid's 527" clause holds. The "KEAX ≤ ~250" line from the analysis
  doc reads 257 on this ruler (region itself reads 264, see §10.4).
- B: material win by spot-check flip — the hybrid SMOOTHS the Moore
  mesocyclone couplet (ΔV 103.5 → 56.9); v4's R0 mask preserves it
  (120.7) while still beating hybrid on boundaries.
- D generic metrics: −80% volume boundaries vs hybrid; control probe
  parity. Whole-volume solve 540 ms ≤ 650 budget; amortized per-tilt
  cost ~32 ms vs hybrid's 124 ms.
- E: v4 substantially beats every baseline that has any evidence to use
  (KTLX 93.5% vs 77.5%; KEAX-with-env 82.7% / noenv 95.3% vs 74.1%).

**Verdict — NOT met (honest):**
- **D blob (hard requirement) unfixed.** Root cause measured, not
  guessed: (1) the archived RAP 00Z profile at the KMBX site is deep
  warm-sector southerly (v +7…+25 m/s at all levels) while the blob
  sector sits behind the gust front — the site-profile env anchor
  actively endorses the wrong branch there (§4a's premise fails for
  storm-modified sectors); (2) broad low-quality velocity swaths
  manufacture smooth fold-0 continuity between the blob and the
  correctly-branched field, surviving interval splitting, vote-share,
  support, and jump-ambiguity guards alike. Every engine ties at +13.3.
  Likely paths forward: 2-D environmental fields (interface change) or
  reflectivity/SNR gate pre-filtering before segmentation.
- **E ≥ 99% unmet** (93.5% best). At N = 12 the branch spacing (24 m/s)
  is comparable to real storm-scale deviation from the RAP profile, so
  the env anchor misassigns exactly where flow deviates most — the §4a
  margin argument does not survive N = 12 with a 13-km model profile.
  KEAX-E env (82.7) < noenv (95.3) is the same effect measured.
- **C volume total regresses vs hybrid** (19715 vs 11174; lowest tilt
  522 vs 1288 still wins). The v4 upper-tilt solves on Ida's split-cut
  VCP inflate seam counts; unresolved at freeze.
- **Graceful degradation (§10.3 "v4-noenv ≥ hybrid on A–D") holds only
  partially** (B: 205 vs 127; D: 3808 — the no-anchor temporal chain
  remains the weak module).

Implementation deviations from this spec (each documented at the code
site): `TemporalPrior::Volume` is solved with the v4 engine itself, not
v1 (a v1 prior re-injects the errors the temporal term corrects);
interval splitting (§4f) is applied inside the v4 tilt segmentation
(load-bearing for F2, not just hygiene); the env unary's per-node label
margin is capped at 4 m/s (one correlated observation must not outvote
a storm-scale anchor once per node); temporal unaries are
decisiveness-gated like the hybrid's dense-reference overrides; strong
edges additionally require an unambiguous mean jump (±0.3 fold);
evidence-free components re-anchor to their highest-Nyquist node
(UNRAVEL's least-aliased initialization); R2 patches carry a per-patch
boundary audit; R3/R4 defer without a covering reference.


---

## 15. Verification addendum (2026-07-02, independent adversarial pass)

The full battery was reproduced end-to-end by an independent verifier
(downloads included; all engines byte-deterministic; data-destruction audit
clean: 0 gates lost, 0 invented, all moves whole-2N). Corrections and
carries the section-14 self-report understated:

- **Case C max-inbound spot check passed BY A DEFECT, not by skill**: the
  recorded max eyewall inbound -155.1 m/s (and couplet 134.1) is a
  physically absurd fold-3 artifact on a NaN-bounded noise island (raw
  -16, every other engine -16, environmental profile -24.3). Do not cite
  either number. The artifact class needs an isolated-island guard before
  promotion.
- **Case C multifold-speckle spot check is NOT met**: 14 connected
  components smaller than 32 gates (spec requires coherent regions;
  hybrid produces 0). Belongs on the section-14 NOT-met list with D-blob
  and E>=99%.
- **The prior-without-environment configuration is dangerous**: with
  temporal caching wired but no environmental profile, v4 is worse than
  hybrid on Case B (205 vs 127) and catastrophically worse on Case D
  (3,808 vs 164 boundaries). The app wiring MUST NOT enable temporal
  propagation before environmental profiles, or must gate v4 selection on
  profile availability.
- **Single-tilt low-Nyquist volumes exceed the 150 ms tilt budget**
  (565 ms on synthetic Case E); the section-10.5 first-paint
  partial-volume solve is unimplemented. Address before live-feed wiring.

**PROMOTION GATE (verifier blocker, standing until lifted):** v4 ships as
an additional selectable engine only. It does not become the default and
is not marketed as anything beyond what section 14 + this addendum can
defend, until: D-blob resolved (2-D environmental fields or
reflectivity/SNR pre-filtering - apply_reflectivity_gate_filter exists),
Case E >=99% met or the requirement renegotiated, the C upper-tilt
volume-total regression fixed, and the no-env degradation chain made
not-worse-than-hybrid on every case.

**Promotion-gate status after v4.1 (2026-07-02, the Py-ART network-
reduction graft — mechanism and full delta table in
`docs/dealias-external-baselines.md` §7; `v4.1*` rows in
`docs/dealias-v4-baselines.json`):**

- **C upper-tilt volume-total regression: RESOLVED.** 19,715 → 1,901
  (hybrid 11,174; lowest tilt 522 → 450). The no-env and cold arms fix it
  too (19,658 → 1,403; 20,139 → 1,553).
- **No-env degradation chain: PARTIALLY RESOLVED.** The catastrophic
  member (D-blob no-env, 3,808 / 26,178) is dissolved: 140 / 1,013 —
  better than hybrid (164 / 11,620) and equal to v4.1-with-env on the
  boundary ruler. The no-env arm now beats hybrid on B/C/D volume totals
  and D-control. Still worse than hybrid: B lowest tilt (205 vs 127) and
  A volume total (10,589 vs 7,064). The verifier's wiring order
  (environmental profiles before temporal caching) stands.
- **Case E >= 99%: NOT met, materially closer.** Best is now the NO-ENV
  arm: E-keax12 96.93% (was 95.25; env arm 82.73 → 83.57), E-ktlx12
  92.36% no-env (was 77.01) / 95.52% env (was 93.52). Py-ART's 99.07
  remains ahead; the remaining gap is no longer an environmental-anchor
  failure but residual segmentation/merge decisions.
- **D-blob: UNCHANGED** (+13.3 all engines, all arms) — reframed by §17 as
  an open problem for the field (ORPG fails it identically); remediation
  paths are reflectivity/SNR pre-filtering and radar-derived low-level
  references, not model profiles.
- New in v4.1, on the record: the repair-echo confidence fix
  (REPAIR_CHANGED < temporal reference floor) breaks cross-volume error
  replication measured on A's SAILS pair; couplet 120.7 preserved in every
  arm; whole-2N/determinism/gate-count invariants re-verified (16/16 runs
  byte-identical). Honest regressions: A cold-start volume total 5,301 →
  8,602, and the synthetic single-tilt N = 12 solve roughly doubles
  (571 → 1,147 ms on E-keax12) — the section-10.5 first-paint item, which
  was already failing, worsens and still blocks live-feed wiring.


## 16. Owner decisions (2026-07-02, post-verification)

- **Environmental profiles: RAP ONLY. NO GFS.** (Amended 2026-07-02 after
  the HRRR rerun measured HRRR = RAP across the battery and refuted the
  3-km-fixes-the-blob hypothesis: same boundary counts on every case,
  blob probe unchanged. RAP wins on operations - ~10x smaller profile
  fetches, domain covers Alaska WSR-88Ds where HRRR-CONUS does not, and
  ONE model source is simpler to maintain per spec 12b. Original
  decision was HRRR-first/RAP-fallback; superseded.)
  International radars run v4 without a profile (the no-env degradation
  path) or stay on the Region engine - the picker copy must be honest
  about which anchor a site actually gets. This also means the no-env
  path's Case-C-class upper-tilt behavior matters for intl users; weigh
  that when deciding whether v4 is offered for intl sites at wiring time.
- Wiring order stands as the verifier required: environmental profiles
  land BEFORE temporal caching, never the reverse.
- Product direction after v4 wires and survives field use: hybrid is
  deleted (its temporal idea lives on as v4's prior term), cascade goes
  with it (its only production consumer is hybrid) - the app returns to
  TWO engines: Region (fast honest fallback) and v4.

**Wiring record (2026-07-03, v0.29.0):**

- Environmental profiles are LIVE: `crates/app_ui/src/dealias_env.rs`
  fetches the RAP 0-h analysis (`awp130pgrb` f00, `noaa-rap-pds` AWS
  mirror) via the existing rusty-weather stack — `rustwx_models` URL
  resolution + `rustwx_io::fetch_bytes_with_cache` `.idx`-subset
  byte-range fetch (UGRD/VGRD/HGT records only) into the same cache root
  the model-data ingest uses. Profiles cache per (site, cycle) for the
  session; fetches run on one background thread and never block
  rendering (renders proceed no-env until the profile lands, then the
  `dealias_env_ptr` in the render keys invalidates exactly the affected
  rasters). Cycle choice: nearest published cycle to the displayed
  volume time (live or archive), sequential fallback across cycles
  within the engine's 3 h staleness guard.
- CONUS only, per the decision above: a bounding-box pre-gate plus a
  20 km nearest-grid-point guard; non-CONUS and international sites run
  the no-env path, and the sidebar shows which anchor the current volume
  actually has.
- The picker slot formerly labeled "3D + time (beta)" is now
  "Analyst 3D (model-anchored)" and runs v4.1. The in-memory engine
  toggle keeps its historical name (`dealias_cascade`) so any state that
  selected the retired hybrid engine maps forward to v4. (The bool was
  never persisted to settings files; nothing on disk needed migration.)
- `hybrid.rs` (950 lines) and `cascade.rs` (251 lines) are DELETED from
  render2d, with the hybrid-only dense-reference pass in the region
  resolver (~150 lines). Two survivors, both still consumed:
  `fit_range_band_reference` (moved into `render2d/src/lib.rs`; feeds
  the battery's `rms_harmonic` metric and the region engine's
  external-reference mode) and `dealias_velocity_grid_with_reference`
  (the region engine's public external-reference entry).
- The bench battery's `cascade`/`hybrid` arms are removed; requesting
  them now errors with a pointer here. `docs/dealias-v4-baselines.json`
  keeps their historical rows for the record — do not delete them.
- Per §15's ordering law, cross-volume `V4VolumeSolution` reuse
  (temporal solution caching) remains OUT OF SCOPE for v0.29.0: the app
  calls `dealias_velocity_grid_v4` per volume with the same
  previous-volume argument the hybrid call sites passed, plus the
  environmental profile. Known cost, stated honestly: the per-cut entry
  point runs a whole-volume solve, so first render of each additional
  tilt of one volume re-solves (~150-550 ms on battery volumes); the
  render caches absorb repeats. The amortized per-volume solution cache
  is the natural post-v0.29.0 follow-up alongside temporal caching.


## 17. External baselines (2026-07-02) - the SOTA question, measured

Full table + method: docs/dealias-external-baselines.md (metric port
cross-validated integer-exact against the Rust harness; both externals
byte-deterministic). Findings that update this spec's directions:

- **Py-ART region_dealias beats v4 on the boundary ruler on every real
  case at equal reference RMS, and wins Case E outright (99.07% with NO
  environmental input)** via its `centered` global-mean recentering.
  That mechanism is the named STEAL for dealias_v4/solve.rs - highest
  expected-value improvement on the promotion-gate list.
- **UNRAVEL's low boundary counts are bought with storm-scale damage**:
  Moore couplet smashed to 40.5 m/s (worst measured; v4 preserves
  120.7), KEAX peak inbound clipped -78 to -41.8, Case E KEAX 28.25%
  (catastrophic). v4's do-no-harm caps are vindicated - do not chase
  UNRAVEL's boundary numbers by relaxing them.
- **The KMBX blob is an OPEN PROBLEM FOR THE FIELD: NOAA's operational
  ORPG fails it identically** (IEM-archived RIDGE render, 23:54Z: same
  wrong AWAY branch; the 23:40Z control is correct). v4 ties the
  operational state of the art there. The section-15 "2-D environmental
  fields" remediation is REFUTED (an HRRR profile at the blob's own
  lat/lon still endorses the wrong branch; the analysis cold pool is
  ~100 m deep vs the 132 m beam). Remaining credible paths:
  reflectivity/SNR pre-filtering before segmentation; radar-derived
  low-level references.
- 4DD is infeasible on native Windows (TRMM RSL has no Windows build);
  documented, not pursued.

Honest positioning until the steal lands: v4 is the only engine
measured that combines competitive residual aliasing with couplet
preservation, confidence output, determinism, and interactive runtime -
"the best engine for storm analysis" is defensible; "best in existence"
is not (Py-ART region holds the boundary crown).

## 18. Case H: the Amory couplet (2026-07-03, OPEN on all engines)

Owner field report (v0.29.0-rc). KGWX20230325_033846_V06 - the
Amory/New Wren EF3 (the Rolling Fork supercell's second violent
tornado, between Okolona and Aberdeen MS). Prior volume 033149; env
fixture `env_kgwx.json` (RAP 04z, strong southerly LLJ: v = 22.7 m/s by
300 m ARL). Also shipped as the "Amory EF3" in-app data pack
(`amory-2023-kgwx`).

**Failure signature:** on the 0.48-deg MESO-SAILS velocity tilts at
03:42:09 (sweep 10) and 03:44:02 (sweep 17), the inbound (west) side of
the tornado couplet is left ONE FOLD HIGH by every engine we have:
the pinned probe (az 282 deg, 28 km) reads +13.2 m/s on region, v4, AND
v4-noenv, where the true field is ~-35 m/s inbound (Nyquist 24.1;
-35 + 2x24.1 = +13.2 - the exact one-fold signature). RadarScope's
archived render of the same tilts unfolds this side correctly, so it is
solvable. The ambient RAP wind is nearly cross-beam at that azimuth
(radial component ~-2 m/s), which is why the environmental unary does
not rescue it: the error is storm-scale (mesocyclone inflow), an order
of magnitude beyond the ambient prior.

Battery rows recorded in `docs/dealias-v4-baselines.json` under
`H-amory` (region bnd_low 1672 / rmsE 27.3; v4 728 / 11.8; v4-noenv
14,193 - the env anchor is load-bearing on this volume). Reproduction
commands and the iteration workflow for whoever attacks this:
`docs/dealias-iteration-guide.md`. Fix wanted on v4 first, region if it
falls out; the do-no-harm rule of s16 applies - no other case's row may
regress beyond noise.
