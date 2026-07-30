# Radar command line

BowEcho's radar CLI is the headless path for inspecting and rendering observed
radar volumes in automated verification workflows. It initializes neither
eframe nor a GPU and reads source files without modifying them.

The primary target is NEXRAD Archive II / Level II. The same production input
router also accepts BowEcho-supported CfRadial, ODIM HDF5, DORADE, and JMA
containers when their format can be identified from the file bytes.

## Inspect a volume

```powershell
bowecho radar inspect C:\data\KTLX20250402_000257_V06 --json
```

The versioned JSON report identifies the decoded format, source receipt,
site/time/VCP metadata, every elevation cut, available moments, and gate
geometry. Cut indices are zero-based and are the exact indices accepted by
`radar render`.

## Render verification rasters

Render every available base moment on its lowest compatible cut:

```powershell
bowecho radar render C:\data\KTLX20250402_000257_V06 `
  --out C:\verification\ktlx --json
```

Select moments and cuts explicitly:

```powershell
bowecho radar render C:\data\KTLX20250402_000257_V06 `
  --out C:\verification\ktlx `
  --moment REF --moment VEL --moment RHO `
  --cut-index 0 --cut-index 2 `
  --width 1200 --height 1200 --json
```

Use `--all-cuts` instead of `--cut-index` to render every compatible cut.
Moment names use BowEcho's stable short IDs: `REF`, `VEL`, `SW`, `ZDR`, `RHO`,
`PHI`, and `KDP`. A requested moment that is absent is recorded as unavailable;
it is never synthesized or replaced by a different field.

`DVEL` (BowEcho whole-volume v4 dealiased velocity) and `CREF` (composite
reflectivity) are available as explicit derived product IDs. CLI `DVEL` does
not use temporal or model-wind anchors; that provenance and each derivation
method are named in the artifact manifest.

The output is a deterministic transparent polar radar raster using BowEcho's
production moment renderer and color tables. It deliberately does not fetch a
basemap. Each PNG and the exact decoded source receive byte-count and SHA-256
receipts in `radar-artifact-manifest.json`.

Artifact paths are confined to `--out`; BowEcho refuses parent traversal and
linked or reparse-point directories beneath that root.

## Verify receipts

```powershell
bowecho radar verify C:\verification\ktlx\radar-artifact-manifest.json
```

Verification recalculates source and artifact hashes, confirms artifact path
containment, checks PNG encoding and dimensions, and emits a versioned JSON
report. A nonzero exit status means the receipt cannot be trusted.

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | Success |
| 2 | Invalid command or option |
| 3 | Missing or inaccessible input |
| 4 | Unreadable or inconsistent radar data |
| 5 | Receipt or artifact verification failed |
| 6 | Requested host capability is unavailable |
| 10 | Internal failure |

## Verification boundary

These commands make observed-radar inputs reproducible for model verification.
They do not yet run WRF along an observed volume's exact rays and acquisition
times. BowEcho's application science already supports that replay path; a
future CLI command can expose it without approximating the geometry in a
second implementation.
