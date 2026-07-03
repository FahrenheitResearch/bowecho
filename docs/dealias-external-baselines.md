# Dealias v4 vs external state of the art — measured baselines

Date: 2026-07-02.  Branch `v028/unslop`.  Companion to
`docs/dealias-v4-spec.md` (§10 battery, §14 results, §15 verifier addendum,
§16 owner decisions).  Numeric rows merged into
`docs/dealias-v4-baselines.json` (engines `pyart-region`, `unravel`,
`v4-hrrr`, `orpg-l3-spot`).  Harness: `crates/bench/py/` (README there has
the exact reproduction steps and pinned versions).

External engines:

- **pyart-region** — Py-ART `dealias_region_based`, shipped defaults
  (Helmus & Collis 2016, *JORS* 4(1):e25, doi:10.5334/jors.119; region
  lineage Jing & Wiener 1993).
- **unravel** — UNRAVEL, default strategy, alpha 0.8, 3-D pass on
  (Louf et al. 2020, *JTECH* 37, 741–758, doi:10.1175/JTECH-D-19-0020.1;
  github.com/vlouf/dealias @ `8e2fc63`).
- **4DD** (James & Houze 2001, *JTECH* 18, 1674–1683) — **not run**:
  Py-ART's `dealias_fourdd` binds the TRMM RSL C library, which has no
  native-Windows build (conda-forge `trmm_rsl` is linux-64/osx-64 only;
  `pyart._rsl_interface` absent from the Windows wheel).  Running it on
  another OS would break the same-machine comparison; documented instead of
  burned hours.
- **orpg-l3-spot** — NOAA's own operational dealiaser (WSR-88D ORPG VDA),
  read from archived Level-III products (see §4).

## 1. Metric parity (the credibility spine)

The Python metric port (`crates/bench/py/dealias_metrics.py`) was validated
against fields dumped by the Rust harness (`bowecho-bench --dealias
--dump-fields`) before any external number was recorded:

| check (case A, engines region and v4; case E rewrap, region) | result |
|---|---|
| boundary pairs, lowest tilt + all 17 cuts summed | exact match (264/2869, 257/5059) |
| speck count, multifold gates + speckle | exact match |
| % gates modified | match to 3 decimals |
| rms_env, rms_harmonic, couplet ΔV, max inbound | match at reported precision (<0.01) |
| correct-branch % (Case E rewrap) | match (74.08) |
| env-wind projection port vs dumped Rust projection | max abs diff 0.000000 m/s, NaN masks identical |

Fairness decisions for the externals (documented in `run_external.py`):
same Level-II files, velocity-bearing sweeps only, per-sweep Nyquist handed
to UNRAVEL explicitly (its default silently applies the first ray's Nyquist
volume-wide — wrong on split-cut VCPs), gatefilter = invalid-velocity gates
only, outputs re-masked to the input gate set (invented-gate count was 0
for every engine on every case anyway).  Both externals were byte-identical
across repeated runs on every case (determinism `true` in every row).

## 2. The shared-metric table

bnd = residual fold-boundary pairs (lowest tilt / volume total, |Δv| >
1.2·N); rmsE = RMS vs the RAP env fixture on the lowest tilt (v4-hrrr rows
measure vs the HRRR fixture — not directly comparable); cplt = max
azimuthal gate-to-gate ΔV 15–40 km; inb = max inbound; cb% = Case-E
correct-branch.  Rust rows from `docs/dealias-v4-baselines.json` (best-of-3,
Ryzen 9 9950X3D); external rows same machine, Python (apples/oranges — §5).

### A — KEAX derecho (super-res, N = 24.1)

| engine | bnd low/vol | rmsE | specks | cplt | inb | time (vol) |
|---|---|---|---|---|---|---|
| region | 264 / 2869 | 8.80 | 81 | 39.8 | −78.0 | 158 ms |
| hybrid | 527 / 7064 | 8.71 | 137 | 39.8 | −69.8 | 943 ms |
| v4 (RAP) | 257 / 5059 | 8.82 | 84 | 39.8 | −75.0 | 540 ms |
| v4 (HRRR) | 257 / 5077 | 8.94* | 84 | 39.8 | −75.0 | 361 ms |
| **pyart-region** | **190 / 2253** | 8.76 | 96 | 37.0 | −64.8 | 13.3 s (py) |
| **unravel** | **84 / 465** | 8.72 | 54 | 37.0 | **−41.8 (clipped)** | 8.3 s (py) |

### B — KTLX Moore EF5 (couplet preservation is the point, N = 26.1)

| engine | bnd low/vol | cplt ΔV | inb | note |
|---|---|---|---|---|
| region | 84 / 1706 | 103.5 | −84.0 | |
| hybrid | 127 / 4601 | **56.9 (smoothed)** | −45.2 | §14 failure |
| v4 | 96 / 2986 | **120.7 (preserved)** | −84.0 | R0 mask |
| **pyart-region** | 63 / 1045 | 98.0 (preserved) | −58.7 | |
| **unravel** | 4 / 117 | **40.5 (worst smoothing measured)** | −32.2 | |
| orpg-l3-spot | — | 65.0 | −45.0 | N0U 20:16Z, 1°×0.25 km — azimuthal resolution halves gate-to-gate ΔV vs super-res; not comparable, listed for the record |

### C — KLIX Ida eyewall (multi-fold, split cuts N = 23.2/32.1)

| engine | bnd low/vol | rmsE | mod% | inb | multifold gates/speckle |
|---|---|---|---|---|---|
| region | 298 / **1047** | 14.49 | 27.7 | −85.7 | 86 / 30 |
| hybrid | 1288 / 11174 | 13.57 | 28.0 | −68.4 | 0 / 0 |
| v4 (RAP) | 522 / 19715 | 13.87 | 28.0 | −155.1 (KNOWN ARTIFACT — do not cite, §15) | 83 / 14 |
| v4 (HRRR) | 523 / 19854 | 13.22* | 28.0 | −155.1 (same artifact) | 83 / 14 |
| **pyart-region** | **92** / 1389 | 13.89 | **14.0** | −76.2 | 0 / 0 |
| **unravel** | **17 / 494** | 13.80 | **14.0** | −66.2 | 0 / 0 |
| orpg-l3-spot | — | — | — | −65.0 | N0U 16:32:52Z; −65.0 sits at the product's encoding floor (7 gates saturated) → read as "≥ 65 m/s inbound" |

The externals modify HALF as many gates as every Rust engine (14% vs 28%)
at essentially equal rmsE, and make zero |fold| ≥ 2 moves on a case chosen
*because* it contains genuine multi-fold velocities.  Ground truth is
unavailable; the honest reading is "under-unfolding vs over-unfolding is
unresolved on C", and the ORPG floor reading (≥65 inbound) cannot separate
region's −85.7 from unravel's −66.2.

### D — KMBX misbranch pair (F3/F5 regression; the blob)

| engine | blob bnd low/vol | probe blob (truth ≈ −39) | probe sector | control bnd | probe ctrl (truth ≈ −33) |
|---|---|---|---|---|---|
| region | 137 / 1176 | +13.3 wrong | +7.0 | 98 / 322 | **−34.0 ✓** |
| hybrid | 164 / 11620 | +13.3 wrong | +7.0 | 123 / 564 | −34.0 ✓ |
| v4 (RAP) | 140 / 2314 | +13.3 wrong | +7.0 | 97 / 372 | −34.0 ✓ |
| v4 (HRRR) | 140 / 2314 | **+13.3 wrong** | +7.0 | 97 / 372 | −34.0 ✓ |
| **pyart-region** | 101 / 427 | +12.1 wrong (= raw; untouched) | +9.0 (= raw) | 48 / 272 | −20.0 (mixed-branch window) |
| **unravel** | 38 / 161 | +12.1 wrong (= raw; untouched) | +9.0 (= raw) | 37 / 100 | −20.7 (mixed-branch window) |
| **orpg-l3-spot** | — | **AWAY = wrong** (N0S SRM render 23:54Z, all 9 px) | AWAY = wrong | — | **TOWARD = correct** (23:40Z, all 9 px) |

(The externals' +12.1/+9.0 equal the raw field probed on their own lattice —
verified — i.e. neither engine moved a single blob/sector gate.  Their −20
control reads are 5×5 windows straddling branches; the branch flip itself
succeeded, partially.)

### E — synthetic low-Nyquist rewrap (N = 12, cold start, exact truth)

| engine | E-keax12 cb% | E-ktlx12 cb% |
|---|---|---|
| region / cascade / hybrid | 74.08 | 77.45 |
| v4 (RAP env) | 82.73 | 93.52 |
| v4 (no env) | 95.25 | 77.01 |
| v4 (HRRR env) | 83.35 | — (2013: no HRRR archive) |
| **pyart-region** | **99.07** | **96.86** |
| **unravel** | **28.25 (catastrophic)** | 92.63 |

pyart-region is the only engine that clears the spec's ≥99% bar on
E-keax12 — with no environmental input at all.  Its `centered` global
recentering (shift the solved fold pattern so the unfolded field's mean sits
near zero) plus 3-way interval splitting is an absolute-branch heuristic
that beats our environmental anchor at N = 12, where §14 already measured
the env term misassigning exactly where flow deviates most.  UNRAVEL's
28.25% on E-keax12 (40.8% of gates moved, rmsE 15.9) is a full-blown
runaway of its box/least-squares modules at low Nyquist.

## 3. HRRR-anchored rows (the resolution experiment)

HRRR 3-km 0-h analysis fixtures (`env_*_hrrr.json`, extracted by
`crates/bench/py/hrrr_profile.py` via `.idx` byte-range subsets of
`noaa-hrrr-bdp-pds`; ~85 MB per cycle instead of ~700 MB).  KEAX
2026-06-09 06Z, KMBX 2026-06-10 00Z, KLIX 2021-08-29 16Z.  The 2013 Moore
case predates the HRRR archive (2014-09-30) and stays RAP-only.

These rows exist to answer "does 3-km resolution change any branch
decision?" — they are an experiment, not a shipping path: on the strength
of the answer below (plus fetch size and Alaska coverage), the owner
amended spec §16 to **RAP-only** production anchoring.

Result: **v4(HRRR) is metric-for-metric indistinguishable from v4(RAP)** —
A: 257 boundaries both; D-blob: 140 both, blob probe +13.3 both; D-control:
−34.0 both; C: 522→523; E-keax12: 82.73→83.35%.  The 3-km anchor neither
helps nor hurts branch decisions on this battery — which, together with
~10× smaller profile fetches and RAP's Alaska coverage, is why production
anchoring is now RAP-only (§16 as amended).

**The KMBX blob dissection (priority spot check).**  Projected radial
velocity of each profile at the probe gates' beam heights (4/3-earth, m/s;
truth at the blob is ≈ −39):

| profile | blob az339/20 km (beam ≈ 132 m ARL) | sector az316/21 km (≈ 140 m) | ctrl az202/57.9 km (≈ 511 m) |
|---|---|---|---|
| RAP 13-km at site | **+11.1 (wrong branch)** | +8.5 | −15.3 (correct side) |
| HRRR 3-km at site | **+8.1 (wrong branch)** | +5.5 | −17.0 |
| HRRR 3-km at the blob's own lat/lon (48.561, −100.962) | **+8.7 (wrong branch)** | +7.4 | −12.8 |

(a) HRRR *does* change the profile materially — but only in the lowest
~100 m: its 10-m wind at the site is post-frontal (u +9.0, v −1.8) where
RAP still carries southerly.  By the first pressure level (~150 m ARL) HRRR
is right back to strong warm-sector southerly (v +11.5), and the probe
beams sit at 130–140 m.  (b) No probe verdict flips.  Crucially, the same
extraction **at the blob's own location** still endorses the wrong branch —
so the §15 "2-D environmental fields" remediation path would NOT have fixed
this case with an HRRR 0-h analysis: the model's cold pool is ~100 m deep
(or misplaced/late at 00Z) where reality had ≈ −39 m/s inbound at 130 m.
The remaining credible paths for the blob are reflectivity/SNR gate
pre-filtering (the low-CC/low-SNR swaths that manufacture the fold-0
continuity) and radar-derived (not model) low-level references.

## 4. ORPG operational comparison (sources and honesty limits)

- **KTLX 2013 / KLIX 2021**: raw Level-III digital base velocity (product
  99, N0U) from `gs://gcp-public-data-nexrad-l3` (NCEI mirror, daily
  per-site `.tar.Z`) and the `unidata-nexrad-level3` S3 bucket.  Read with
  Py-ART; quantitative spot values in §2.  Level-III is 1° × 0.25 km vs
  super-res 0.5° — gate-count metrics are NOT comparable and are not
  reported.
- **KMBX / KEAX 2026**: no public raw Level-III exists (the Unidata bucket
  spans ~2020-03→2023, the GCS mirror ends ~2025-12, NCEI is order-only).
  The only public record of the ORPG's 2026 decisions is IEM's archived
  RIDGE render of N0S (storm-relative mean velocity), georeferenced by its
  world file.  Branch reads are QUALITATIVE (TOWARD/AWAY) and defensible
  only because the candidate branches differ by ~2N ≈ 52 m/s, far beyond
  any plausible storm-motion offset baked into SRM.
- **Verdicts**: control az202/57.9 km @23:40 = solid green, TOWARD,
  correct (matches every engine's −34).  Blob az339/20 km and sector
  az316/21 km @23:54 = red family, AWAY, **wrong branch — NOAA's
  operational dealiaser fails the blob exactly like every other engine
  tested.**  Pixel evidence: blob RGBA (0.596, 0.467, 0.467), sector
  (0.635, 0, 0), ctrl (0.024, 0.561, 0.012); 9/9 pixel votes each.

## 5. Runtime (report, don't lean)

Rust rows: release, best-of-3, amortized volume solve — region 158 ms /
v4 ~540 ms for the KEAX volume.  Python rows: best-of-2 after JIT warmup,
same machine — pyart-region 2.1–13.3 s, UNRAVEL 6.2–10.8 s per volume.
Python-vs-Rust is apples/oranges (interpreter overhead vs compiled, numba
JIT residue, no isolation); the honest claims are only: (1) all engines are
fast enough for research use; (2) the Rust engines are the only ones inside
an interactive-app budget; (3) UNRAVEL and pyart-region runtimes are the
same order of magnitude as each other.

## 6. Verdict (sober)

**Nothing solves the KMBX blob.** Not v1–v4 with RAP or HRRR anchors (site
or storm-relative extraction point), not Py-ART's region engine, not
UNRAVEL, and — the one genuinely new fact this exercise produced — **not
NOAA's own operational ORPG dealiaser**, which shipped the same wrong
outbound branch in its public products at 23:54Z.  The blob is not a
"v4 isn't SOTA yet" gap; it is an open problem for the entire field.  The
§15 promotion-gate item should be reframed accordingly: v4 matching the
operational state of the art on D-blob is a tie, not a failure — but the
gate's remediation list should drop "2-D environmental fields" (measured
dead end here, §3) in favor of reflectivity/SNR pre-filtering and
radar-derived low-level references.

**Where v4 leads all externals, measured**: mesocyclone-couplet
preservation (B: 120.7 vs pyart 98.0, unravel 40.5, hybrid 56.9 — the R0
mask is the only explicit couplet guard in the field and it shows);
low-Nyquist correctness *when evidence exists* (E-ktlx12 93.5% before
externals; pyart edges it at 96.9); deterministic volume-joint solve with
confidence output (no external engine produces confidence at all); and
interactive runtime.

**Where an external engine beats v4, measured — and what to steal**:
Py-ART `dealias_region_based` posts lower residual-boundary counts than v4
(and than our region engine) on every real case at equal reference RMS
(A: 190 vs 257; C: 92 vs 522; D-blob: 101 vs 140; B: 63 vs 96) and wins
Case E outright (99.07% with zero environmental input vs v4's 82.7/95.3).
The mechanism worth stealing is its **`centered` global-branch recentering**
(post-solve, shift the whole fold assignment toward zero volume-mean) plus
finer interval-limit segmentation — the natural landing spot is a
low-weight global-mean unary in `crates/render2d/src/dealias_v4/solve.rs`'s
energy (it would have supplied exactly the absolute-branch symmetry breaker
that Case E's env anchor gets wrong at N = 12).  Caveat kept honest: part
of pyart's boundary advantage may be buying smoothness the boundary metric
can't see (§1.4 finding 2), but its couplet (98.0) and Case-E truth scores
say it is NOT doing so pathologically.

**UNRAVEL** produces the prettiest boundary counts in the table (4 / 117 on
Moore) and the worst measured storm-scale damage: Moore couplet smoothed to
40.5 (below even the hybrid failure the battery was built to catch), KEAX
RIJ max inbound clipped −78 → −41.8, and a catastrophic 28.25% on
E-keax12.  Its window-ladder repair idea is already in v4 (R3/R4) — the
measured lesson is that v4's do-no-harm caps and couplet mask around those
modules are load-bearing, not decorative.

Production context: the shipping configuration this table vouches for is
**v4 with the RAP site profile** (§16 as amended; the HRRR rows are the
experiment that justified staying on RAP).  Bottom line: "literally the
best dealias algo in existence" is **not supported** as a blanket claim — Py-ART's region engine is ahead on the
battery's own boundary/Case-E rulers.  The defensible claim after this
exercise: v4 is the only engine tested that combines competitive residual
aliasing with *measured* storm-feature preservation, confidence output,
determinism, and interactive speed — and on the hardest case in the
battery it ties the operational state of the art (everyone loses).  Steal
Py-ART's global-mean anchor for the solve energy, keep the couplet mask,
and re-run this table.
