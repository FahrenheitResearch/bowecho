# External dealias baselines (Python harness)

Companion to `bowecho-bench --dealias` (see `../README.md` and
`docs/dealias-v4-spec.md` §10): runs EXTERNAL state-of-the-art dealiasers on
the same battery cases, scored with a **parity-validated port** of the exact
metric definitions in `crates/bench/src/dealias_eval.rs`, plus the
operational-ORPG spot checks and the HRRR fixture extractor.  Results and the
verdict live in `docs/dealias-external-baselines.md`; the numeric rows are
merged into `docs/dealias-v4-baselines.json`.

## External engines and primary references

- **pyart-region** — `pyart.correct.dealias_region_based`: Helmus, J. J., and
  S. M. Collis, 2016: The Python ARM Radar Toolkit (Py-ART). *J. Open Res.
  Softw.*, 4(1), e25, doi:10.5334/jors.119.  Region segmentation lineage:
  Jing, Z., and G. Wiener, 1993, *JTECH* 10, 798–808.
- **unravel** — Louf, V., A. Protat, R. C. Jackson, S. M. Collis, and
  J. Helmus, 2020: UNRAVEL: A robust modular velocity dealiasing technique
  for Doppler radar. *JTECH*, 37, 741–758, doi:10.1175/JTECH-D-19-0020.1.
  Reference implementation github.com/vlouf/dealias (pinned commit in
  `requirements.txt`).
- **dealias_fourdd (4DD)** — James, C. N., and R. A. Houze Jr., 2001: A
  real-time four-dimensional Doppler dealiasing scheme. *JTECH*, 18,
  1674–1683.  **Not run — infeasible on native Windows**: Py-ART's
  `dealias_fourdd` is a binding over the TRMM Radar Software Library (RSL),
  which Py-ART only builds when RSL is present; conda-forge packages
  `trmm_rsl` for linux-64/osx-64 only and the library itself is
  POSIX-oriented C with no Windows port.  `pyart._rsl_interface` is absent
  from the Windows build (verified in this env).  Running it would need a
  Linux host/WSL, which would also invalidate the same-machine runtime
  comparison.  Documented rather than pursued.
- Metric internals ported from Rust: Browning, K. A., and R. Wexler, 1968
  (*J. Appl. Meteor.* 7, 105–113) per-range-band harmonic reference;
  Doviak, R. J., and D. S. Zrnić, 1993 (*Doppler Radar and Weather
  Observations*, 2nd ed., eq. 2.28b) 4/3-earth beam height for the
  environmental-wind projection (James & Houze 2001 sounding-anchor idea).

## Environment (reproducible)

Windows (used for the baseline rows; conda-forge for the eccodes binary):

```
conda create -p <env> -c conda-forge python=3.11 arm_pyart=2.2.4 scipy numba \
    cfgrib xarray requests
<env>/python -m pip install "unravel @ git+https://github.com/vlouf/dealias@8e2fc63e95686c311b7e2efd9f99fb4c02305eac"
```

Linux/macOS: `pip install -r requirements.txt` should suffice.  When invoking
`python.exe` without conda activation on Windows, set
`ECCODES_DIR=<env>\Library` (and put `<env>\Library\bin` on `PATH`) so
python-eccodes finds the DLL — only `hrrr_profile.py` needs it.

## Workflow

1. **Dump reference fields from the Rust harness** (bench never fetches;
   volumes per `../README.md`):

   ```
   bowecho-bench --dealias --target KEAX20260609_055143_V06 \
     --prior KEAX20260609_054454_V06 --env ../fixtures/dealias/env_keax.json \
     --engines region,v4 --iters 1 --json --dump-fields dumps/A > rust_A.json
   ```

2. **Metric parity (the credibility gate — run before citing anything):**

   ```
   python parity_check.py --dump-dir dumps/A --rust-json rust_A.json \
     --engine region --env-fixture ../fixtures/dealias/env_keax.json
   ```

   Integer metrics must match exactly, floats to the printed precision, and
   the env-projection port is checked gate-for-gate against the dumped Rust
   projection.  Validated 2026-07-02 on case A (engines `region` and `v4`,
   17 cuts) and on the Case-E rewrap (correct-branch %): all exact,
   projection max |diff| = 0.000000 m/s.

3. **External engines** on a case volume (or on a `--rewrap` dump for the
   synthetic Case E):

   ```
   python run_external.py --volume KMBX20260609_235423_V06 --case D-blob \
     --env-fixture ../fixtures/dealias/env_kmbx.json \
     --probe 339,20,blob --probe 316,21,sector --out ext_D-blob.json
   python run_external.py --rewrap-dump dumps/E12 --case E-keax12 \
     --env-fixture ../fixtures/dealias/env_keax.json --out ext_E-keax12.json
   ```

   Fairness decisions (per-sweep Nyquist for UNRAVEL, velocity-validity
   gatefilter, velocity-sweeps-only radar, output re-masked to the input's
   gate set) are documented at the top of `run_external.py`.

4. **HRRR fixtures** (the resolution experiment behind the amended §16
   decision to anchor production on RAP only):

   ```
   python hrrr_profile.py --volume KEAX20260609_055143_V06 \
     --cycle 2026-06-09T06 --out ../fixtures/dealias/env_keax_hrrr.json
   ```

   Fetches only the UGRD/VGRD/HGT records via `.idx` byte ranges (~85 MB vs
   ~700 MB full file) from `noaa-hrrr-bdp-pds`.  The HRRR archive begins
   2014-09-30, so the 2013 Moore case is RAP-only by necessity.

5. **ORPG operational spot checks** (`orpg_probe.py`):

   - raw Level-III digital base velocity (product 99 / N0U family) read via
     Py-ART — QUANTITATIVE probes; sources: `unidata-nexrad-level3` S3
     bucket (covers ~2020-03 → ~2023) and the NCEI mirror
     `gs://gcp-public-data-nexrad-l3` (1992 → ~2025-12, daily per-site
     `.tar.Z`, extract with bsdtar);
   - IEM-archived RIDGE renders of N0S (storm-relative velocity) with world
     files — QUALITATIVE branch reads (TOWARD/AWAY) for dates with no public
     raw Level-III (the 2026 cases; NCEI is order-only there).  Never
     compare L3 gate-count metrics against super-res Level-II — resolution
     differs (1° × 0.25 km vs 0.5° × 0.25 km); spot checks only.

6. **Assemble** the external rows into `docs/dealias-v4-baselines.json` and
   print the markdown table: `python assemble_results.py --results-dir <dir>`.

## Determinism

Each external engine is run twice on identical input and compared
bitwise (NaN mask included).  Observed 2026-07-02: **pyart-region and
unravel both byte-deterministic on every case** (`"deterministic": true` in
every row).  The Rust engines' determinism is enforced separately by the
bench's own exit-code gate.

## Runtime caveat

Timings from this harness are wall-clock of the Python dealias call
(NumPy/SciPy/numba inside), best-of-2 after a JIT warmup run, on the same
machine as the Rust rows — but Python-vs-Rust runtime comparisons remain
apples/oranges (different memory models, JIT warmup excluded, no
process-level isolation).  Report them; do not lean on them.
