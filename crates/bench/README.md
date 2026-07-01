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
