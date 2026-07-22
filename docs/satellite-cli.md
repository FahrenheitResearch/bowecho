# Satellite command line

BowEcho's satellite CLI catalogs and acquires native provider history, then
inspects, renders, and verifies canonical satellite rw-store runs without
initializing the desktop UI or GPU. GOES, Himawari, Meteosat/MTG, and SimSat
share the same ingest, decoder, store, and native plotting paths as the
Satellite window.

A run directory has this shape:

```text
<sat-store>/<model>/<run>/
  run.json
  grid.rwg
  tHHMM.rws
```

Pass the exact run directory. The CLI does not guess which account, cache, day,
or satellite the caller intended.

## List native archive frames

Every archive request has an inclusive UTC range and an explicit positive
result cap:

```powershell
bowecho satellite list --source goes --satellite goes19 --sector conus `
  --product c13 --start 2026-07-21T18:00:00Z --end 2026-07-21T19:00:00Z `
  --limit 24 --json
```

The `bowecho.satellite.catalog.v1` JSON report contains the normalized request,
provider-advertised bounds where available, chronological scan timestamps, and
the exact native object IDs, URLs, and known byte counts. The newest matching
frames are retained when the cap is reached; `truncated: true` means additional
matches exist.

UTC must be written as `Z` or `+00:00`. BowEcho imposes no artificial earliest
date. Provider holdings and live capabilities are authoritative. A missing
provider bound means unknown, not unlimited.

| Source | Satellites | Sector | Products |
| --- | --- | --- | --- |
| `goes` | GOES-16 through GOES-19 (`g16`/`goes16`, etc.) | Required: `conus`, `fulldisk`, `meso1`, or `meso2` | `c01`-`c16`; native ABI RGB style slugs |
| `himawari` | Himawari-8 or -9 (`h8`, `h9`) | Full disk; omit or use `fulldisk` | `b01`-`b16`, `true_color` |
| `meteosat` | MTG-I1 / Meteosat-12 (`mtg_i1`, `m12`) | Advertised full-disk WMS extent | `geo_colour`, `true_colour`, `ir_105_hrfi`, `vis_06_hrfi`, `cloud_phase`, `cloud_type`, `dust`, `fire_temperature`, `fog_low_cloud`, `snow`, `lightning_afa` |

Source aliases `noaa_goes`/`abi`, `noaa_himawari`/`ahi`, and
`mtg`/`eumetsat` are accepted. GOES listing is mode-agnostic (historical M3 and
M6 filenames are parsed), and a composite appears only when all required ABI
channels exist. Himawari frames require every numbered native segment for all
required bands. Meteosat times, bounds, and cadence come from live EUMETView
capabilities; its products are provider-rendered geolocated RGB.

## Fetch native archive frames

```powershell
bowecho satellite fetch --source himawari --satellite h9 --sector fulldisk `
  --product b13 --start 2026-07-21T18:00:00Z --end 2026-07-21T19:00:00Z `
  --store-root C:\verification\sat-store --max-frames 12 --json
```

BowEcho catalogs before creating the store root or downloading data. If the
selection is empty or exceeds `--max-frames`, fetch stops without writing. It
uses the exact immutable objects from that catalog snapshot and never
synthesizes missing scans or exceeds the cap.

The `bowecho.satellite.fetch.v1` report records each attempted frame, its
canonical run directory, model/run/HHMM key, stored bytes, warnings, and
failures. Partial or failed acquisition returns a nonzero data exit. Existing
canonical runs remain readable and may receive additional frames.

GOES and Himawari archive ingest currently uses a decode stride of four.
Single bands retain physical scalar values: AHI B01-B06 are reflectance and
B07-B16 are Kelvin. Native composites and Meteosat products are stored as RGB.

## Inspect a stored run

```powershell
bowecho satellite inspect `
  "$env:APPDATA\BowEcho\sat-store\g18\conus_c13_20260721" --json
```

The versioned JSON report validates the run identity, grid, frame metadata,
variables, selectors, dimensions, and frame-to-grid hash agreement. The
`HHMM` value is a storage key; scan timestamps reported by the stored selector
remain authoritative.

## Render frames

Render the latest stored frame:

```powershell
bowecho satellite render <RUN-DIR> --out C:\verification\sat --json
```

Render one or more explicit storage times:

```powershell
bowecho satellite render <RUN-DIR> --out C:\verification\sat `
  --frame 1930 --frame 1940 --width 1600 --height 1200 --json
```

`--frame all` renders every declared frame. Scalar bands use the same physical
palette and map plotter as BowEcho's Satellite window. Stored RGB composites
remain RGB, and SimSat derived fields keep their fixed product palettes. The
renderer does not recover numeric radiance from baked RGB imagery.

The artifact manifest identifies whether a source is observed scalar imagery,
observed baked RGB, or simulated output. Exact `grid.rwg` and selected
`tHHMM.rws` inputs plus every PNG receive byte-count and SHA-256 receipts.
`--out` must be a real directory outside the source run; linked/junction paths
and parent traversal are refused.

## Verify receipts

```powershell
bowecho satellite verify C:\verification\sat\satellite-artifact-manifest.json
```

Verification recalculates all declared hashes, enforces artifact containment,
and confirms PNG encodings and dimensions. It does not contact a provider.

## Automation and exit codes

Machine JSON is written to stdout and progress or diagnostics to stderr; use
`--json` explicitly in automation. Exit codes are stable: `0` success, `2`
usage, `3` input/store path, `4` data or partial acquisition, `5` verification,
`6` provider unavailable, and `10` internal failure.

Network acquisition remains owned by BowEcho's existing satellite engine; the
CLI is orchestration around that same science path, not a reduced reimplementation.
