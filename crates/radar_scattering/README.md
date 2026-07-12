# radar_scattering

`radar_scattering` is the non-UI science boundary for future property-aware
radar forward operators. Phase 1 provides:

- validated additive ZH, ZV, complex HH/VV covariance, KDP, AH, AV, and
  ZH-weighted terminal-speed moments;
- a fallible field-for-field conversion to the quantities currently consumed
  by `app_ui`'s `PolarAccumulator`;
- distinct conventional, P3, and ISHMAEL particle states and provenance;
- deterministic, validated grid-point closures from raw WRF scalars to those
  states, with per-property provenance;
- explicit kernel, orientation, melting, temporal-sampling, and validation
  metadata;
- a schema-v1 offline LUT with exact units, SHA-256 integrity, strict
  rectilinear axes, and deterministic multilinear interpolation; and
- a three-dimensional body-frame amplitude transform into the radar H/V
  basis.

The crate does **not** read/select WRF fields, integrate a full PSD, average an
orientation distribution, calibrate amplitudes into reflectivity, integrate
propagation along a ray, or expose UI. Those are later integration layers.

## WRF property closure v1

`closure` accepts one already-unit-normalized grid-point scalar tuple. Missing,
non-finite, negative, cross-scheme, and internally inconsistent inputs fail;
the WRF adapter must never substitute a field. Every output property is tagged
as a native-prognostic derivation, preferred WRF diagnostic, documented
closure, or assumption.

- P3 50/51 use `QICE`, `QNICE`, `QIR`, and `QIB`. P3-52 additionally permits a
  second tuple named `QICE2`, `QNICE2`, `QIR2`, and `QIB2`. P3-53 requires
  advected `QZI=(N*Z)^0.5` and recovers the native bulk sixth moment exactly as
  `M6=Z=QZI^2/QNICE`; characteristic size then uses the distinct number-mean
  `D6=M6/QNICE`. It does not diagnose liquid fraction.
- P3 first validates `0 <= QIR <= QICE`, then bounds the numerical ratio
  `QIR/QICE` to `[0,1]`. Positive rime mass requires positive rime volume and
  rime density is `QIR/QIB`. Closure-v1 effective density is the explicitly
  labeled constituent-volume value
  `QICE / (QIB + (QICE-QIR)/917 kg m^-3)`. The non-M6 characteristic diameter
  is spherical mass-equivalent diameter. Aspect and fall speed are documented
  analytic proxies and are not claimed to reproduce a P3 lookup table.
- ISHMAEL 55 retains the three actual WRF prognostic tuples: unsuffixed
  planar-nucleated ice, suffix `2` columnar-nucleated ice, and suffix `3`
  aggregate ice. Each requires its matching `QICE`, `QNICE`, `QVOLI`, and
  `QAOLI`; `SmallIce` and `Rimed` remain physical closure states, not invented
  WRF tuples. Present `d_ice`, `rho_ice`, `phi_ice`, and `v_ice` diagnostics
  (with the same suffix) take field-by-field precedence; an invalid present
  diagnostic fails rather than falling back. Exact resolved variable names
  are retained in property and record provenance. Otherwise density is
  `QICE/QVOLI`; the separately labeled `QAOLI/QVOLI` volume-weighted metric is
  interpreted by closure v1 as shape input but is never represented as the
  `phi_ice` diagnostic.
- P3 and ISHMAEL do not predict canting. Scheme-default orientation is an
  assumed zero-mean Gaussian: P3 standard deviation broadens from 10 to 40
  degrees with rime fraction; ISHMAEL small/planar/columnar use 10 degrees and
  aggregate/rimed use 40 degrees. Aligned Gaussian and true isotropic
  overrides are explicit metadata choices.

### `DiagnosticCoexistenceV1`

Melting is deliberately diagnostic, never scheme-native. It is available only
when positive rain and frozen mass coexist from 268.15 through 275.15 K. Let
`t` be temperature's bounded linear position in that envelope, `F` total
frozen mass, and `R` rain mass. The liquid needed to reach the temperature
limit is `F*t/(1-t)` (unbounded at the warm edge), paired liquid is the lesser
of that value and `R`, and actual wet fraction is
`paired/(F+paired)`. Paired liquid is distributed in the original frozen
category fractions. Therefore:

`unused_rain + sum(wet_category_mass) = R + F`

Rain is removed from the separate remainder exactly once. Density, aspect,
Gaussian canting parameters (when both endpoints are Gaussian), and fall speed
transition linearly by diagnosed wet mass fraction toward the rain values.
Homogeneous-mixture and water-coated-frozen-core topology hooks are retained,
but both explicitly report `NotEvaluatedNoLutOrAmplitude`; the diagnosis does
not invent T-matrix amplitudes or a LUT result.

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
