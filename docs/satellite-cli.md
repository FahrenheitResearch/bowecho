# Satellite command line

BowEcho's satellite CLI renders already-ingested satellite and SimSat frames
without initializing the desktop UI. It consumes the same canonical rw-store
runs used by the Satellite window, so GOES, Himawari, Meteosat/MTG, and SimSat
share one decoder and native plotting path.

A run directory has this shape:

```text
<sat-store>/<model>/<run>/
  run.json
  grid.rwg
  tHHMM.rws
```

Pass the exact run directory. The CLI does not guess which account, cache, day,
or satellite the caller intended.

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

## Scope

This first CLI surface is intentionally local-store-first. Network acquisition
and raw ABI NetCDF or Himawari HSD ingestion remain owned by BowEcho's existing
satellite engine; they are not reimplemented as reduced CLI-only science.
