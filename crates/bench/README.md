# bowecho-bench

Headless benchmark harness for BowEcho's decode + raster hot path. No
window, no network, no `app_ui` dependency — just the same `nexrad_io`
decode entry and `render2d` viewport-raster path the app runs, timed
with `std::time::Instant` and checksummed for byte-identical output.

## The three purposes

1. **LTO A/B referee.** Build the bin under two link configurations
   (e.g. `lto = "fat"` vs `lto = "thin"`), run both against the same
   volume, and compare the per-stage numbers. The pixel checksum in the
   output must match between the two builds — if it does not, the
   comparison is measuring different work and is void.
2. **x86-64-v3 validation.** Build with
   `RUSTFLAGS="-C target-cpu=x86-64-v3"` (or a specific `target-cpu`)
   and confirm both that the binary runs on the target machine and that
   the checksum matches the baseline build: same pixels, only faster.
3. **PGO training workload.** The workload deliberately walks the
   shipping hot path — bzip2-chunked Archive II decode, palette build,
   velocity dealiasing, and rotated viewport gate sampling at three
   real display resolutions — so `-C profile-generate` runs of this bin
   produce profiles representative of what the app spends its time on.

## Usage

```
bowecho-bench <path-to-level2-file> [--iters N] [--json]
```

- `<path-to-level2-file>` — a NEXRAD Archive II / Level-II volume
  (plain, gzip, whole-file bzip2, or LDM bzip2-chunked `.V06`). Any
  other single-buffer format BowEcho decodes (ODIM_H5, CfRadial,
  DORADE, JMA GRIB2 tar) also works: the bench uses the app's shared
  byte router. No station hint is needed; the site ID comes from the
  file contents.
- `--iters N` — timed iterations after the 1 warmup iteration
  (default 10).
- `--json` — emit one machine-readable JSON summary line instead of
  the human table.

Per iteration the harness:

1. decodes the full volume from the in-memory file bytes;
2. rasters the lowest reflectivity cut and the lowest velocity cut
   (dealiased, the app's DVEL display) at 1280x720, 1920x1080, and
   2560x1440 — 0.25 km/px, radar slightly off-center, 20 mrad rotation.

It reports mean/min/max milliseconds per stage plus the per-iteration
total, and an FNV-1a checksum over all rendered pixels (hashed outside
the timed sections). The process exits nonzero if the checksum varies
across iterations, so scripted A/B runs fail loudly instead of quietly
comparing nondeterministic output.

## Getting a canonical volume

The AWS Open Data Level-II archive (`unidata-nexrad-level2`) is public
and needs no credentials. Key pattern:

```
https://unidata-nexrad-level2.s3.amazonaws.com/YYYY/MM/DD/SSSS/SSSSYYYYMMDD_HHMMSS_V06
```

Older archive years (through the mid-2010s) carry a `.gz` suffix on the
key and must be gunzipped to the raw `AR2V` file before benching.

Suggested canonical volume — KTLX during the 2013-05-20 Moore, OK
tornado (dense storm-scale returns, bzip2-chunked, exercises the
dealiaser hard):

```
curl -O https://unidata-nexrad-level2.s3.amazonaws.com/2013/05/20/KTLX/KTLX20130520_201643_V06.gz
gunzip KTLX20130520_201643_V06.gz
```

The bench itself never fetches anything; download the file first and
pass the local path.

## Examples

```
# Human-readable A/B run
cargo run --release -p bowecho-bench -- KTLX20130520_201643_V06 --iters 20

# Machine-readable line for scripts
cargo run --release -p bowecho-bench -- KTLX20130520_201643_V06 --json

# Smoke test of the stage plumbing against a real file
BOWECHO_BENCH_FILE=KTLX20130520_201643_V06 \
  cargo test -p bowecho-bench -- --ignored smoke
```

Benchmark discipline: compare builds with the same profile
(`--release`), same file, same machine, on AC power, and prefer `min`
over `mean` when the machine is noisy.

## Dealias eval battery (`--dealias`)

The second mode runs the dealias-engine battery from
`docs/dealias-v4-spec.md` §10: every engine (region / v4 / v4-noenv) on
one case volume, with residual fold-boundary pairs,
reference RMS (environmental + Browning & Wexler harmonic), % gates
branch-modified, isolated speck count, 5×5-gate branch spot-checks,
best-of-N runtime, and a two-run byte-identical determinism gate
(nonzero exit on drift).

The `cascade` and `hybrid` engines (and their battery arms) were removed
at v0.29.0 (dealias-v4 spec §16); `docs/dealias-v4-baselines.json` keeps
their historical rows.

```
bowecho-bench --dealias --target KEAX20260609_055143_V06 \
  --prior KEAX20260609_054454_V06 \
  --env crates/bench/fixtures/dealias/env_keax.json \
  --probe 339,20,blob --iters 3 [--json]
```

`--rewrap 12` runs the synthetic low-Nyquist Case E instead: the lowest
velocity tilt's accepted v4 output becomes exact truth, re-wrapped to
±12 m/s and presented as a single-tilt cold-start volume; engines are
scored on % correct-branch gates (|v − truth| < 6 m/s).

### Case volumes (bench never fetches — download first)

All from the public `unidata-nexrad-level2` mirror (2013 keys carry
`.gz`; gunzip first):

```
base=https://unidata-nexrad-level2.s3.amazonaws.com
curl -O $base/2026/06/09/KEAX/KEAX20260609_055143_V06   # A derecho (target)
curl -O $base/2026/06/09/KEAX/KEAX20260609_054454_V06   # A prior
curl -O $base/2013/05/20/KTLX/KTLX20130520_201643_V06.gz # B Moore EF5 (target)
curl -O $base/2013/05/20/KTLX/KTLX20130520_201229_V06.gz # B prior
curl -O $base/2021/08/29/KLIX/KLIX20210829_163252_V06   # C Ida eyewall (target)
curl -O $base/2021/08/29/KLIX/KLIX20210829_162629_V06   # C prior
curl -O $base/2026/06/09/KMBX/KMBX20260609_235423_V06   # D blob (target)
curl -O $base/2026/06/09/KMBX/KMBX20260609_234726_V06   # D prior
curl -O $base/2026/06/09/KMBX/KMBX20260609_234055_V06   # D positive control
curl -O $base/2026/06/09/KMBX/KMBX20260609_233434_V06   # D control prior
```

### Environmental fixtures

`fixtures/dealias/env_*.json` are `EnvironmentalWindProfile` fixtures
hand-extracted once from archived RAP 13-km 0-h analyses at each radar
site and volume time (provenance in each file's `source` field; AWS
`noaa-rap-pds` for 2021/2026, NCEI historical archive for 2013).
Heights are meters above radar level. Cases must also run WITHOUT the
fixture to measure graceful degradation (spec §10.1).
