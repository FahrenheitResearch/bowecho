# Reproducible PyTMatrix 0.3.3 research generator

This directory contains an executable, locked generator for schema-v1 radar
scattering LUTs. Every output is `research_only_unvalidated` in the crate
header and `unvalidated_research` in the external manifest. Nothing here
activates a production path.

The generator currently covers three **conventional**, monodisperse particle
tables in `research_only_assets/tmatrix/pytmatrix-0.3.3`:

- homogeneous liquid-water rain spheroids;
- homogeneous dry-ice spheroids; and
- homogeneous Maxwell-Garnett wet-hail spheroids.

It does not represent P3 or ISHMAEL properties. It does not infer a PSD or WRF
category mapping. Each electromagnetic node is normalized to exactly one
particle per cubic metre, so all nine stored components remain additive.

## Locked environment

`Dockerfile` pins platform-specific CPython 3.11.9 and Rust 1.85.1 image
manifests, an immutable Debian snapshot, native package versions, hashed Python
wheels, the full upstream PyTMatrix release commit, and its downloaded archive
SHA-256. NumPy/SciPy BLAS linkage, the compiled Fortran extension, native
packages, compiler versions, and executable hashes are captured in the emitted
`environment.json`.

PyTMatrix 0.3.3 uses `numpy.distutils`; its build requires `setuptools < 60`.
The official PyPI sdist and GitHub release ZIP omit the required
`pytmatrix.pyf`. The pinned source therefore comes from the official 0.3.3 tag
commit rather than those incomplete packaging artifacts. Exact failures and
hashes are retained in `FAILURE_RECORD.md`.

The image build runs the fail-closed upstream gate:

```sh
python -m unittest pytmatrix.test.test_tmatrix
```

## End-to-end command

From the repository root on Windows with Docker Desktop running:

```powershell
powershell -ExecutionPolicy Bypass -File crates/radar_scattering/tools/pytmatrix-0.3.3/run_all.ps1
```

The script builds the locked image, captures the environment, generates all
three LUTs, runs analytic/sphere/resonance sanity checks, computes direct
PyTMatrix held-out nodes in fresh processes, and writes the interpolation-only
report. The exact expanded commands are in `run_all.ps1`; the built image ID
and result are recorded in `reproduction_run.json`.

To generate only one already-configured table inside the image:

```sh
python crates/radar_scattering/tools/pytmatrix-0.3.3/generate_lut.py generate \
  --config research_only_assets/tmatrix/pytmatrix-0.3.3/conventional_wet_hail_sband_unvalidated/config.json \
  --output research_only_assets/tmatrix/pytmatrix-0.3.3/conventional_wet_hail_sband_unvalidated/table.lut \
  --manifest research_only_assets/tmatrix/pytmatrix-0.3.3/conventional_wet_hail_sband_unvalidated/manifest.json \
  --environment-report research_only_assets/tmatrix/pytmatrix-0.3.3/environment.json \
  --overwrite
```

## Deterministic and fail-closed behavior

`generate_lut.py`:

1. reads exact bytes, rejects invalid UTF-8, duplicate JSON keys and
   `NaN`/`Infinity`, then hashes those unmodified bytes;
2. requires table-specific material, geometry, orientation, shape,
   normalization, terminal-velocity, process, and payload declarations;
3. maps crate `minor_to_major_axis_ratio` to PyTMatrix's
   horizontal-to-rotational ratio (`1/q` for oblate, `q` for prolate);
4. runs every grid point in a fresh Python process with one numerical thread;
5. rejects the whole table after any timeout, native exit, nonfinite value, or
   schema invariant failure;
6. collects points in declared axis order with the last axis fastest; and
7. asks the locked Rust `brslut-emitter` to construct and serialize
   `OfflineLut`, then independently cross-checks the exact header, config,
   payload bytes, and their SHA-256 digests before the atomic rename.

The canonical output order is ZH, ZV, complex HH/VV covariance real/imaginary,
KDP, AH, AV, and ZH-weighted first/second fall-speed moments. Backscatter is
computed in PyTMatrix horizontal backscatter geometry. KDP and attenuation use
horizontal forward geometry. Covariance magnitude/phase come from PyTMatrix
`rho_hv` and `delta_hv`, with the configured HH × conjugate(VV) convention.

Frozen dry-ice and wet-hail tables use PyTMatrix's fixed-order orientation
average with a 20-degree Gaussian canting distribution, five alpha points and
ten Gautschi-derived beta points (50 deterministic angle pairs per particle).
The liquid-rain table retains explicitly fixed vertical symmetry axes.

PyTMatrix does not supply terminal speeds. Rain uses the explicitly configured
Atlas et al. exponential law over its stated diameter range. Dry/wet hail uses
the configured Schiller-Naumann gravity/drag iteration. The stored moments are
always `ZH*v_t` and `ZH*v_t^2`; nonzero reflectivity is never paired with zero
placeholder moments.

## Scientific scope and validation status

The dielectric constants are explicit, table-specific narrow-S-band values at
273.15 K. Frequency is a configurable schema axis restricted to 2–4 GHz, but
these supplied configs contain a single 2.700832954954955 GHz node (111 mm) and
declare their refractive indices constant over configured nodes. Wet hail is a
homogeneous ice-host/water-inclusion Maxwell-Garnett approximation with mass
fraction converted to volume fraction using component densities. It is not a
coated-hail or laboratory-validated morphology model.

The first coarse-grid interpolation attempt is preserved as
`validation/tmatrix/initial_grid_design_failure_report.json`; every table
failed its predeclared thresholds, so those nodes became grid-design/tuning
data and the coarse LUTs were discarded. The shipped diameter grids use at
most 1.25 spacing (1.20 for wet hail below 25 mm and about 1.10 above it) and
contain 48 rain, 87 dry-ice and 575 wet-hail points. The wet table also uses
five liquid-fraction and five shape-ratio coordinates to resolve the
resonance-region interpolation failure preserved in the v2 report.

`validation/tmatrix/held_out_interpolation_report.json` compares multilinear
LUT interpolation with direct PyTMatrix evaluations at nodes absent from the
LUT. Because both paths share this generator, PyTMatrix, dielectric models,
orientation, and velocity closures, that report checks generator/interpolation
behavior only. It is deliberately not attached as
`held_out_validated` science metadata.

The final off-grid nodes are selected only after grid bytes are frozen by
`select_held_out_nodes.py`, using a public seed and SHA-256-derived within-cell
fractions without evaluating scattering. The production gate remains failed
until independent Mie, PSD integration,
covariance/basis, propagation-unit, and wet-particle evidence exists and is
reviewed. Upstream self-tests and the shipped Rayleigh sanity test do not meet
that bar.
