# Non-production PyTMatrix 0.3.3 research assets

Everything in this directory is an explicitly non-production research
artifact. Each table directory contains the exact UTF-8 `config.json` embedded
in `table.lut` and a `manifest.json` with complete LUT, header, config, payload,
environment, generator, and tool-source SHA-256 identities. The shared
`environment.json` records the locked native runtime; `reproduction_run.json`
records the top-level command, image ID, table identities, counts, and reports.

The conventional tables are:

- `conventional_liquid_rain_sband_unvalidated`;
- `conventional_dry_ice_spheroids_sband_unvalidated`; and
- `conventional_wet_hail_sband_unvalidated`.

The view-aware property-coordinate bundle is:

- `property_p3_ishmael_dry_oblate_sband_unvalidated`;
- `property_p3_ishmael_dry_prolate_sband_unvalidated`;
- `property_p3_ishmael_wet_oblate_sband_unvalidated`;
- `property_p3_ishmael_wet_prolate_sband_unvalidated`; and
- `property_rain_sband_unvalidated`, used for standalone rain and for residual
  rain after liquid paired into wet frozen categories is removed exactly once.

Every electromagnetic node is monodisperse and normalized to exactly 1 m^-3.
The property bundle uses exactly 2.8 GHz, separate oblate/prolate shapes,
Gaussian20 orientation with deterministic 5-by-10 quadrature, temperature-
dependent dielectric formulas, porous/wet symmetric Bruggeman mixing, and a
radar-elevation axis spanning -0.5 through 20 degrees. Property configs use
exactly `ddelt=0.001` and `ndgs=14`. The final property bundle contains
2,204,496 Cartesian points: 163,520 dry-oblate, 132,160 dry-prolate, 1,017,600
wet-oblate, 864,000 wet-prolate, and 27,216 rain points. Role-specific dense
diameter grids preserve solver-complete domains at shape ratio 0.1: 89 mm dry
oblate, exactly 32.31174267785264 mm dry prolate, 15 mm wet oblate, and 6.3 mm
wet prolate. The dry-prolate cap prevents interpolation across an unresolved
large/slender-particle KDP resonance; rejected domains, order/ddelt probes,
node-removal failures, and lower-order convergence failures are retained under
`validation/tmatrix`.

The v2 rain table spans 18 micrometres through 7 mm and 225 through 313.15 K,
covering the audited supercooled rain and complete WRF-P3 `get_rain_dsd2`
lambda-limited size state without a clamp, omission, or Rayleigh substitution.
Its stored normalized fall moments use the same positive Schiller-Naumann law.

Frozen Schiller-Naumann terminal speeds use bracketed force balance and an
explicit Re=1000 boundary selection when the piecewise drag-coefficient jump
straddles zero. Stored normalized fall moments follow that declared generator
law; runtime property evaluation replaces them with closure-derived moments.

The property coordinates can consume bounded characteristic-particle states
closed from P3/ISHMAEL-style inputs. They do not reproduce either scheme's
native PSD, do not expose rime axes independently of closure-derived density
and shape, and do not constitute full P3 or ISHMAEL validation.

All headers remain `research_only_unvalidated`. Strict runtime loading,
held-out interpolation, analytic sanity, and Build-24/default-beam view checks
are software evidence, not operational calibration or independent science
validation. Extrapolation and silent fallback are forbidden.
