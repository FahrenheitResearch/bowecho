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

Human, one directory:

```text
bowecho wrf render C:\forecast\case-17 \
  --preset severe \
  --out C:\artifacts \
  --run-manifest C:\forecast\case-17\run.json \
  --workers 8 \
  --json > artifact-manifest.json
```

Automation may pass several paths as distinct arguments, avoiding shell
concatenation, or pass a quoted glob for BowEcho to expand itself:

```text
bowecho wrf render "/forecast/case-17/wrfout_d0*" \
  --preset severe --out /artifacts --run-manifest /forecast/case-17/run.json \
  --variable W_UP_MAX --variable QGRAUP --json
```

Inputs are expanded without a shell, symlinks are not followed, and the final
file list is sorted and deduplicated. A glob may match filenames within one
literal directory; pass the directory itself for safe recursive discovery.
`--variable` is repeatable and requests a generic native field in addition to
the preset. A missing preset or requested field must be recorded as unavailable;
it must not be fabricated.

The initial preset name is `severe`. The public `RenderOptions` contract keeps
the parsed input set, output root, run manifest, worker cap, variables, and JSON
mode intact for the BowEcho application host. That host owns rendering so the
CLI cannot drift into a second WRF decoder or plotting implementation.

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
does not delete or modify the source or its producer-owned marker.

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
`wrf_process -> rw-store -> Rusty Weather batch renderer` pipeline; this layer
intentionally contains no second decoder, diagnostic implementation, or image
renderer.
