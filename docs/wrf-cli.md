# BowEcho WRF command line

BowEcho's shipping binary now dispatches an exact `bowecho wrf` prefix before
the desktop or GPU stack starts. An ordinary `bowecho <file>` invocation still
opens that file in the desktop app.

## Inspect WRF and GPUWM output

```text
bowecho wrf inspect /forecast/run --json > inspect.json
```

The input may be one regular file or a directory. Directory discovery is
recursive, deterministic, does not follow symlinks, and considers wrfout names,
common NetCDF/HDF5 extensions, and files with a supported container signature.
Each source is streamed through SHA-256 and inspected with BowEcho's pinned
Rust-native `netcrust` and `wrf-core` readers.

The `bowecho.wrf.inspect.v1` result reports domains, valid times, dimensions,
spacing, projection, vertical levels, variables, units, staggering,
initialization/global attributes, bytes, SHA-256, and missing or suspicious
metadata. `complete` currently means the container metadata, structural WRF
dimensions, and WRF `Times` were readable and mutually consistent; it does not
claim every multi-gigabyte variable payload was decoded.

## Render a forecast batch

Human, one directory (PowerShell):

```powershell
bowecho wrf render C:\forecast\case-17 `
  --preset severe `
  --out C:\artifacts `
  --run-manifest C:\forecast\case-17\run.json `
  --workers 8 `
  --json > artifact-manifest.json
```

Automation may pass several paths as distinct arguments, avoiding shell
concatenation, or pass a quoted glob for BowEcho to expand itself:

```text
bowecho wrf render "/forecast/case-17/wrfout_d0*" \
  --preset severe --out /artifacts --run-manifest /forecast/case-17/run.json \
  --variable T2 --variable W_UP_MAX --json
```

Inputs are expanded without a shell, symlinks are not followed, and the final
file list is sorted and deduplicated. A glob may match filenames within one
literal directory; pass the directory itself for safe recursive discovery.
`--variable` is repeatable and requests a generic two-dimensional native or
BowEcho-derived field in addition to the preset. Three-dimensional native
fields require an explicit vertical-level contract that this first version
does not yet expose, so they fail clearly instead of silently choosing a
level. A missing preset or requested field is recorded as unavailable; it is
never fabricated.

The initial preset name is `severe`. The public `RenderOptions` contract keeps
the parsed input set, output root, run manifest, worker cap, variables, and JSON
mode intact for the BowEcho application host. The shipping `bowecho` binary
routes the job through the same `wrf_process -> rw-store -> Rusty Weather`
pipeline used by the desktop app; the CLI does not carry a second decoder,
diagnostic engine, or plotter.

Every compatible run/domain/grid group is processed independently. Internal
WRF `Times` records are authoritative, including sub-hourly output; an ordinal
rw-store slot is never reported as a forecast hour. Plots are native-domain
1200 x 900 PNGs under the deterministic artifact paths below.

The first severe preset requests:

- composite reflectivity and accumulated/interval precipitation;
- 2-m temperature/dewpoint, MSLP with 10-m wind, and 10-m gust when present;
- native SB/ML/MU CAPE and CIN when present;
- 2-5 km updraft helicity and column-maximum vertical velocity; and
- native accumulated snow/graupel fields when the file exposes them.

Surface reflectivity and a scheme-native hail product are currently recorded
as unavailable because BowEcho does not yet have a verified lowest-level/1-km
interpolation or a general scheme-native hail contract. A missing optional
field is an explicit `unavailable_products` receipt, not a fabricated plot or
a reason to discard other valid products.

## Watch a live GPUWM/WRF run

```text
bowecho wrf watch /forecast/live \
  --preset severe \
  --out /artifacts \
  --run-manifest /forecast/live/run.json \
  --workers 8 \
  --stable-seconds 30 \
  --poll-seconds 5 \
  --completion-marker .complete \
  --journal /artifacts/watch-state.json \
  --jsonl
```

A candidate becomes eligible only after both of these are true:

1. Its size and modification time remain unchanged for the stable window, or
   its sibling completion marker exists (for example `wrfout_d01.complete`).
2. BowEcho can open it as NetCDF, read its structural metadata, and read at
   least one WRF `Times` record.

The explicit marker accelerates the attempt; it never bypasses the readability
proof. Each work claim is saved through an atomic resume journal. A restart
releases an interrupted claim for retry, while a completed source receipt is
idempotently keyed by normalized path, byte count, and SHA-256. The watcher
does not delete or modify the source or its producer-owned marker. An unchanged
processed file is skipped before another NetCDF read or hash. Unchanged
read/render failures use a bounded retry delay; a changed size/mtime retries
immediately.

The run-level `artifact-manifest.json` is cumulative. Rewriting a source at the
same domain and valid time replaces that scene's receipts instead of retaining
stale products. Each processed source also receives an immutable, hash-addressed
manifest snapshot beside the cumulative manifest, allowing the resume journal
to retain a stable verification receipt while later forecast times arrive.

For a normal producer that writes one valid time per file, the first file has
no predecessor from which to derive interval precipitation. Run-total
precipitation remains available; interval precipitation is emitted when the
same render invocation contains a compatible prior accumulation. Persisting
that predecessor across separate live-watch files is a documented first-version
limitation.

## GPUWM `run.json`

GPUWM should write a strict, versioned manifest before invoking BowEcho:

```json
{
  "schema_version": "bowecho.wrf.run.v1",
  "case_id": "case-17",
  "run_id": "gpuwm-20260722t1800z",
  "member_id": "m01",
  "experiment_track": "gpuwm-corrected",
  "model": "GPUWM WRF",
  "microphysics_scheme": "P3",
  "provenance": {
    "source_commit": "0123456789abcdef",
    "config_commit": "123456789abcdef0",
    "input_commit": "23456789abcdef01",
    "table_commit": "3456789abcdef012"
  }
}
```

Valid experiment tracks are `wrf-parity`, `gpuwm-corrected`, and
`sase-experimental`. Unknown keys, unsafe identifiers, and malformed commit
hashes fail closed. BowEcho hashes the exact `run.json` bytes and carries that
receipt into `bowecho.wrf.artifacts.v1`.

Artifact roots are deterministic:

```text
<out>/<case_id>/<run_id>/<member_id?>/<domain>/<valid-time>/<product>.<extension>
<out>/<case_id>/<run_id>/<member_id?>/artifact-manifest.json
```

Times are normalized to `YYYYMMDDTHHMMSSZ`. Product segments are Windows-safe,
bounded, and receive a digest suffix whenever sanitization could cause a name
collision.

## Verify artifact receipts

```text
bowecho wrf verify /artifacts/case/run/artifact-manifest.json > verify.json
```

The verifier accepts only the `bowecho.wrf.artifacts.v1` contract, validates
required identifiers and receipt shapes, then streams every declared source and
artifact through SHA-256. Artifact paths must remain beneath the manifest
directory. Source paths may be absolute or manifest-relative because raw-data
retention belongs to the external forecast orchestrator.

BowEcho never uploads or deletes source wrfout files. A successful local verify
receipt is designed for the later external workflow:

```text
raw -> processed -> transferred -> remotely verified -> deletable
```

## Automation contract

Machine JSON is written only to stdout. Progress and diagnostics go to stderr.
Stable exit codes are:

| Code | Meaning |
| ---: | --- |
| 0 | Success |
| 2 | Invalid command or arguments |
| 3 | Input path or filesystem failure |
| 4 | Unreadable/incomplete WRF data or invalid manifest JSON |
| 5 | Receipt/schema verification mismatch |
| 6 | Valid command requires an application-host capability not present in this build |
| 10 | Internal serialization/output failure |

The CLI crate owns parsing, contracts, deterministic paths, input expansion,
and watcher recovery. Actual plots remain delegated to BowEcho's existing
production pipeline.

### Agent workflow

An automation agent should treat stdout as a protocol stream and stderr as a
human progress stream:

```text
bowecho wrf inspect /forecast/live --json > inventory.json
bowecho wrf render /forecast/live/wrfout_d01_2026-07-22_18:00:00 \
  --preset severe --variable W_UP_MAX \
  --out /artifacts --run-manifest /forecast/live/run.json \
  --workers 8 --json > render-result.json
bowecho wrf verify /artifacts/case-17/run-01/artifact-manifest.json \
  > verification.json
```

The agent should branch on the process exit code and the manifest `status`,
then inspect `unavailable_products`, `warnings`, and `failures`; it must not
infer success from the presence of one PNG. Artifact and source hashes are the
handoff boundary to a later uploader or retention controller. BowEcho itself
never uploads or authorizes deletion of raw WRF output.

WRF is the first complete programmable vertical slice. Future automation
families belong beside it (`radar`, `satellite`, `sounding`, and `formula`),
using the same rules: stable versioned schemas, deterministic filesystem
artifacts, machine output on stdout, progress on stderr, and no GUI scripting.
