# Non-production PyTMatrix research assets

Everything below this directory is an explicitly non-production research
artifact. None of these files is loaded or activated by application code.

Each table directory contains the exact UTF-8 `config.json` embedded in its
schema-v1 `table.lut` and a `manifest.json` with full LUT-header, config,
payload, environment, generator, and tool-source SHA-256 identities. The shared
`environment.json` records the locked native runtime; `reproduction_run.json`
records the exact top-level command and built container image ID.

All electromagnetic values are conventional spheroids normalized to a
monodisperse number density of exactly 1 m^-3. Frozen dry/wet particles use a
declared 20-degree Gaussian canting distribution with deterministic 5-by-10
PyTMatrix orientation quadrature. The tables are:

- `conventional_liquid_rain_sband_unvalidated`: explicit liquid water;
- `conventional_dry_ice_spheroids_sband_unvalidated`: explicit solid ice; and
- `conventional_wet_hail_sband_unvalidated`: homogeneous Maxwell-Garnett
  ice-host/water-inclusion mixture, including resonance-sized nodes.

The shape ratio is an independent LUT coordinate; these assets do not impose a
diameter-dependent raindrop or hail shape relation. Rain fall speed uses the
configured Atlas exponential law. Frozen-particle fall speed uses a
sphere-equivalent Schiller-Naumann gravity/drag balance with the configured air
density/viscosity and particle or mixture density; no nonspherical projected-
area correction is inferred. Those velocity choices are separate closures,
not PyTMatrix outputs.

All headers remain `research_only_unvalidated`. These assets do not establish
P3 coverage, ISHMAEL coverage, production readiness, independently held-out
science validation, a PSD closure, or a native microphysics-scheme mapping.
