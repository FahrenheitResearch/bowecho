# radar_scattering

`radar_scattering` is the non-UI science boundary for future property-aware
radar forward operators. Phase 1 provides:

- validated additive ZH, ZV, complex HH/VV covariance, KDP, AH, AV, and
  ZH-weighted terminal-speed moments;
- a fallible field-for-field conversion to the quantities currently consumed
  by `app_ui`'s `PolarAccumulator`;
- distinct conventional, P3, and ISHMAEL particle states and provenance;
- explicit kernel, orientation, melting, temporal-sampling, and validation
  metadata;
- a schema-v1 offline LUT with exact units, SHA-256 integrity, strict
  rectilinear axes, and deterministic multilinear interpolation; and
- a three-dimensional body-frame amplitude transform into the radar H/V
  basis.

The crate does **not** select WRF fields, close a PSD, average an orientation
distribution, calibrate amplitudes into reflectivity, integrate propagation
along a ray, or expose UI. Those are later integration layers.

The body-frame API transforms complex amplitude tensors. Schema-v1 LUTs store
radar-basis additive quantities, so they reject `ExplicitBodyFrame` metadata;
the amplitude transform and kernel calibration must happen before LUT emission.

## LUT safety boundary

A decoded `OfflineLut` has passed all of the following checks:

1. file and header magic plus schema agreement;
2. exact canonical output order and units;
3. unique, finite, strictly increasing axes with kind-specific units;
4. generator identity, embedded JSON-config SHA-256, and science metadata;
5. schema-derived payload length and payload SHA-256; and
6. per-node covariance and fall-speed-moment invariants.

Queries name every axis in declared order and never extrapolate. The payload
layout is point-major little-endian f64 with the last declared axis varying
fastest.

## Evidence status

There is no generic "production" label. `TableValidation` distinguishes
synthetic fixtures, unvalidated research tables, and tables accompanied by an
independently held-out validation report and digest. Operational consumers can
call `ScienceMetadata::require_held_out_validation()` as a status gate and
`verify_held_out_report()` against the exact report artifact. SHA-256 provides
integrity and identity here, not provenance authentication by itself.

All numerical LUT fixtures in unit tests are labeled synthetic analytic data.
They test software behavior only and are not physical T-matrix results.

The [`tools/pytmatrix-0.3.3`](tools/pytmatrix-0.3.3/README.md) directory pins a
research generator environment and describes the evidence still required. It
does not contain a generator output table.
