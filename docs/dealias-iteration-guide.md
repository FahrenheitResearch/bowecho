# Dealias iteration guide (for any agent/model attacking a dealias bug)

Written 2026-07-03 for a fresh agent with NO prior context on this
repository. Follow it literally. The current target is **case H (the
Amory couplet, spec §18)** but the workflow applies to any dealias
failure. Read this file, then skim `docs/dealias-v4-spec.md` §10 (metric
definitions), §16 (owner decisions/laws), and §18 (the open failure).

## 1. The problem you are solving (case H)

Volume `KGWX20230325_033846_V06` (Amory MS EF3, 2023-03-25). On the
0.48° velocity tilts at 03:42:09 (sweep 10) and 03:44:02 (sweep 17),
the **inbound (west) side of the tornado couplet is left one fold
high** by every engine in the repo: it renders outbound (+13 m/s red)
where the true field is ~−35 m/s inbound (blue). Nyquist is 24.1 m/s,
so −35 + 2×24.1 = +13.2 — the exact one-fold error. RadarScope's
archived render of the same tilts gets it right, so this is solvable.

Why the current machinery misses it: the aliased inbound sector is
surrounded mostly by outbound air (the couplet's own +45 m/s side and
ambient outbound), so boundary votes favor leaving it folded; and the
RAP ambient wind at that azimuth is nearly cross-beam (radial ~−2 m/s),
so the environmental prior is too weak to overrule the votes. The
error is storm-scale rotation, an order of magnitude beyond the ambient
prior. Any fix likely needs couplet-aware reasoning (e.g. symmetry of a
velocity couplet: strong outbound on one side implies strong inbound on
the other at the same range) or smarter region segmentation around the
rotation.

**Success =** the pinned probe reads ~−35 (anything strongly negative,
< −20, is directionally right; verify visually too) on **v4** (called
"Analyst v4" in-app), ideally also on **region**, with NO other case
regressing (§4 below).

## 2. Repo orientation (only what you need)

- `crates/render2d/src/region_core.rs` — the fast Region fallback.
  Region-based unfolding, Eilts & Smith 1990 lineage.
- `crates/render2d/src/dealias_v4/` — the whole-volume v4.1 engine ("Analyst v4"):
  - `mod.rs` — entry points, super-region segmentation over all tilts;
  - `graph.rs` — boundary-vote graph;
  - `solve.rs` — max-spanning-forest DP + ICM (Besag 1986) over one
    discrete energy: boundary votes + vertical overlap + temporal prior
    + environmental unary (James & Houze 2001 / 4DD lineage);
  - `merge.rs` — gap-bridged region merging (Helmus & Collis 2016 /
    Py-ART lineage) with two do-no-harm gates;
  - `repair.rs` — UNRAVEL-style repair ladder (Louf et al. 2020) with
    couplet freeze (Feldmann et al. 2020);
  - `confidence.rs`, `env_profile.rs` — per-gate confidence, env
    projection.
- `crates/bench/src/dealias_eval.rs` — the battery harness (below).
- `docs/dealias-v4-baselines.json` — every measured row. NEVER edit
  existing rows; append new ones with a date.
- Case volumes live in `C:/Users/drew/radar-work/dealias-cases/`
  (download commands in `crates/bench/README.md`).

## 3. The iteration loop (build → run → read → repeat)

Build the harness (fast, no GUI deps):

```
cargo build --release -p bowecho-bench
```

Run case H (from the repo root; `V` = the case-volume dir):

```
V=C:/Users/drew/radar-work/dealias-cases
target/release/bowecho-bench.exe --dealias \
  --target $V/KGWX20230325_033846_V06 \
  --prior  $V/KGWX20230325_033149_V06 \
  --env crates/bench/fixtures/dealias/env_kgwx.json \
  --probe 282,28,couplet --iters 1 --case H-amory
```

Read the output line per engine:

- `probe couplet` — THE number for case H. Currently +13.2 everywhere;
  you want ~−35. Add more probes with repeated `--probe az,km,label`
  (e.g. probe the outbound side too: it must STAY strongly positive —
  a fix that flips the whole couplet is worse than the bug).
- `bnd_low` / `bnd_vol` — residual fold-boundary pairs (lowest velocity
  tilt / whole volume). Lower is better; big jumps up = new folds.
- `rmsE` / `rmsH` — RMS against the environmental projection / the
  Browning & Wexler harmonic fit. Sanity metrics, not targets.
- `cplt` — couplet max Δv preserved. If your change SMOOTHS the couplet
  (cplt drops), it is destroying the tornado signature: hard fail.
- `spk` — isolated specks; `mf` — |fold|≥2 gates/speckle.
- `det yes` — byte-identical across two runs. `det no` = hard fail,
  the harness exits nonzero.

For pixel-level inspection, `--dump-fields <dir>` writes every decoded
field as f32-LE `.bin` + meta `.json` per engine and cut; the Python
loaders in `crates/bench/py/dealias_metrics.py` read them (numpy). The
two bad tilts are cuts at 0.48° — timestamps 03:42:09 and 03:44:02.

## 4. Do-no-harm: the full battery (run before claiming success)

Re-run EVERY case below with your change and compare against the rows
in `docs/dealias-v4-baselines.json` (`H-amory` too — its `_note` holds
the failure record). No row may regress beyond noise (±5% on
boundaries, cplt within 1 m/s). Commands differ only in files:

| Case | --target | --prior | --env |
|---|---|---|---|
| A derecho | KEAX20260609_055143_V06 | KEAX20260609_054454_V06 | env_keax.json |
| B Moore EF5 | KTLX20130520_201643_V06 | KTLX20130520_201229_V06 | env_ktlx.json |
| C Ida eyewall | KLIX20210829_163252_V06 | KLIX20210829_162629_V06 | env_klix.json |
| D blob | KMBX20260609_235423_V06 | KMBX20260609_234726_V06 | env_kmbx.json |
| D control | KMBX20260609_234055_V06 | KMBX20260609_233434_V06 | env_kmbx.json |
| E low-Nyquist | (case B target) `--rewrap 12` | — | — |
| H Amory | KGWX20230325_033846_V06 | KGWX20230325_033149_V06 | env_kgwx.json |

Also run each WITHOUT `--env` (graceful-degradation rule, spec §10.1)
and WITHOUT `--prior` (cold start). Known open items you are NOT
expected to fix: the D-blob (KMBX) case fails on every engine including
NOAA's operational ORPG — do not chase it; case A cold-start v4 row is
a known regression on the post-release list.

## 5. Rules that are law (spec §16, do not re-litigate)

1. Determinism is a gate: same input → byte-identical output, no
   randomness, no HashMap iteration order reaching results (use BTreeMap
   or sort).
2. The temporal prior is confidence-gated and must never be the ONLY
   thing unfolding a gate that the env profile contradicts.
3. `couplet freeze`: repair passes must not modify gates inside a
   detected couplet mask (that is how Moore's 120.7 m/s Δv survives).
4. Environmental anchoring is RAP-only, CONUS-only; engines must stay
   respectable with `env = None` (v4-noenv rows are measured proof).
5. Maintainability beats cleverness (engine spec §12b): boring explicit
   code, doc comments state invariants, new behavior gets a pinning
   test in the same commit.

## 6. Gates before any commit

```
cargo test -q --workspace          # all green, no exceptions
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Battery determinism (`det yes` on every engine/case) is part of the
definition of done. Record new measured rows in
`docs/dealias-v4-baselines.json` (append, never overwrite) and note the
mechanism + date in `docs/dealias-v4-spec.md` §18.

## 7. Attack ideas (untested, in rough order of promise)

1. **Couplet symmetry unary**: the repair ladder already detects
   couplets (couplet mask in `repair.rs`). A gate adjacent to a strong
   outbound core, at the same range, on the opposite azimuthal side,
   should get an unfolding vote toward the mirrored sign. This directly
   encodes what a human (and apparently RadarScope) uses.
2. **Region split at the couplet**: the folded inbound sector is likely
   being merged into an outbound-dominated super-region before voting;
   splitting regions along the zero-isodop/shear axis near detected
   couplets would let the vote graph unfold it independently.
3. **Range-band harmonic reference** (`fit_range_band_reference` in
   `render2d/src/lib.rs`): at the couplet's range band the harmonic fit
   may already predict inbound at az 282; check why its residual term
   is not outvoting the boundary term — possibly just a weight issue
   near high-shear gates.
4. Compare against Py-ART on this exact volume (workflow + parity
   ports in `crates/bench/py/`, see its README): if Py-ART's
   region-based engine unfolds it, diff which merge decision diverges.

Good luck. Measure first, then change one thing at a time.
