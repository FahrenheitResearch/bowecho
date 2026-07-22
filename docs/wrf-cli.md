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
| 10 | Internal serialization/output failure |

`wrf render` and `wrf watch` are the next delivery stage. They will delegate to
BowEcho's existing `wrf_process -> rw-store -> Rusty Weather batch renderer`
pipeline; this command layer intentionally contains no second decoder or image
renderer.
