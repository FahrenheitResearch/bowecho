# Reproducible PyTMatrix 0.3.3 research generator

This directory contains the locked generator for schema-v1 radar-scattering
LUTs. Every output remains `research_only_unvalidated`; generating or loading
one of these files does not make it production-ready.

The end-to-end run emits eight tables under
`research_only_assets/tmatrix/pytmatrix-0.3.3`:

- three historical conventional monodisperse tables for liquid rain, dry ice,
  and Maxwell-Garnett wet hail; and
- five view-aware property-coordinate tables: dry oblate, dry prolate, wet
  oblate, wet prolate, and rain used both standalone and as the residual after
  liquid mass paired into wet frozen categories is removed exactly once.

The property tables are compatible with bounded, closure-derived P3/ISHMAEL-
style characteristic-particle coordinates. They are not scheme-native PSD
integrals and do not claim full P3 or ISHMAEL scientific validation.

## Exact S/C/X pack generation

`generate_band_pack.py` provides the reproducible five-role external-pack path
for exact 2.8 GHz S, 5.6 GHz C, and 9.4 GHz X research frequencies. It reuses
the property material, ODF, geometry, solver, and execution contracts, calls
this locked generator for every real LUT node, and emits schema-1 `pack.json`
with strict role-file sizes and SHA-256 identities. It never interpolates
frequency. See [`PACK_FORMAT.md`](PACK_FORMAT.md) for the command, layout,
manifest schema, and provenance-hash definitions.

The pack generator always writes `unvalidated_research`. C/X generation does
not reuse S-band convergence evidence and cannot be marked validated by a
command-line switch; independent band-specific convergence and validation
records are required before a separately reviewed manifest can activate a
pack in BowEcho.

## Locked environment

`Dockerfile` pins CPython 3.11.9 and Rust 1.85.1 image manifests, an immutable
Debian snapshot, native package versions, hashed Python wheels, and the full
upstream PyTMatrix 0.3.3 release commit. The image build runs the upstream
`python -m unittest pytmatrix.test.test_tmatrix` gate and builds both the Rust
LUT emitter and the typed runtime-loader smoke executable. NumPy/SciPy BLAS
linkage, the compiled Fortran extension, compilers, packages, executables, and
tool-file SHA-256 identities are captured in `environment.json`.

PyTMatrix 0.3.3 uses `numpy.distutils` and requires `setuptools < 60`. Its PyPI
sdist and GitHub release ZIP omit the required `pytmatrix.pyf`, so the locked
source is the official 0.3.3 tag commit. Exact packaging and native-solver
failures are retained in `FAILURE_RECORD.md`.

## End-to-end command

From the repository root on Windows with Docker Desktop running:

```powershell
powershell -ExecutionPolicy Bypass -File crates/radar_scattering/tools/pytmatrix-0.3.3/run_all.ps1
```

The script rebuilds the locked image, recaptures the environment, verifies the
already-passing refined-grid all-node convergence report against exact
environment/generator/validator/audit/config hashes, generates all eight LUTs,
loads every exact LUT/config/hash tuple through the Rust runtime, runs sanity
checks, selects the final held-out nodes, compares direct PyTMatrix with LUT
interpolation, and checks every supported scan center at center and plus/minus
the default beam sigma. The convergence command is deliberately run and
preserved before this script; `run_all.ps1` never silently recomputes or
replaces that expensive scientific gate. The image ID, table hashes, node
counts, and report hashes are written to `reproduction_run.json`.

## Deterministic and fail-closed behavior

`generate_lut.py`:

1. hashes exact config bytes and rejects duplicate keys, invalid UTF-8, and
   nonfinite JSON numbers;
2. requires table-specific particle, material, geometry, orientation,
   terminal-speed, execution, and payload declarations;
3. maps minor/major ratio `q` to PyTMatrix horizontal/rotational ratio as
   `1/q` for oblate and `q` for prolate spheroids;
4. runs conventional points in crash-isolated processes and property tables in
   fresh crash-isolated material-state groups with at most 12 concurrent
   workers, reusing a T matrix only across declared radar-elevation samples of
   the same particle state and restoring results by flat index regardless of
   completion order;
5. rejects an entire LUT after any timeout, native exit, nonfinite result,
   partial group, or schema invariant failure;
6. stores points in declared axis order with the last axis fastest; and
7. passes both axis coordinates and payload scalars to the locked Rust emitter
   as exact IEEE-754 bit strings, constructs `OfflineLut`, then independently
   checks exact header, config, payload bytes, and SHA-256 digests before an
   atomic rename.

All electromagnetic nodes are normalized to exactly one particle per cubic
metre. The canonical additive output order is ZH, ZV, complex HH/VV covariance
real/imaginary, signed KDP, AH, AV, and ZH-weighted first/second fall-speed
moments. The radar transform uses the PyTMatrix local H/V scattering basis and
turns beam elevation `e` into the declared backscatter and forward geometries.

## Property-coordinate physics and limits

The embedded five-table set uses exactly 2.8 GHz. External research packs may
instead be generated directly at exactly 2.8, 5.6, or 9.4 GHz; one pack always
contains only one of those frequencies. All use a radar-elevation axis spanning
-0.5 through 20 degrees and mean-zero Gaussian canting with 20-degree standard
deviation and deterministic 5-by-10 orientation quadrature. Oblate and prolate
frozen states are separate tables. Frozen minor/major ratio covers 0.1 through
1.0; rain covers 0.5 through 1.0.

Dry states span 190 through 273.15 K and bulk density 1.5 through 917 kg/m3.
They use Matzler (2006) ice permittivity and a passive, symmetric Bruggeman
air/ice mixture. Wet states span 269.15 through 275.15 K, condensed volume
fraction 0.0015 through 1.0, and liquid mass fraction 0 through 0.98. They use
temperature-dependent Liebe-Hufford-Manabe water, ice temperature capped at
273.15 K for phase equilibrium, and a symmetric air/ice/water Bruggeman root
followed continuously from vacuum. Rain spans 250 through 313.15 K using the
same temperature-dependent liquid-water model.

Native PyTMatrix is resolution-sensitive at the joint extreme of large
diameter and minor/major ratio 0.1. The initial `ndgs=2` probes, failed all-grid
`ndgs=8` to `10` comparison, and failed first refined-grid `ndgs=12` to `14`
sweep are retained. Final property configs use exactly `ddelt=0.001` and
`ndgs=14`. Rule-based interpolation refinement preserves the original anchors,
adds diameter midpoints, spaces dry density and wet condensed fraction by
density excess over 1.225 kg/m3 air, and resolves wet liquid-fraction curvature.
The final five-table Cartesian grid contains 2,180,240 points.

Runtime dispatch is phase/shape-specific, so solver-complete domains are
retained separately: dry oblate through 89 mm, dry prolate through
32.31174267785264 mm, wet oblate through 15 mm, and wet prolate through 6.3 mm.
The first refined sweep found three near-zero dry-prolate KDP disagreements at
the inserted 36.12562655 mm node. Removing only that node while retaining the
50 mm domain failed 2,195 interpolation component budgets, and higher-order
probes were pairwise nonmonotonic. The final domain therefore stops at the last
all-grid-converged node so interpolation cannot bridge the unresolved
resonance. Future recovery requires a diagnostic/dimension-expanded PyTMatrix
rebuild or an independent scattering solver to determine the numerical cause;
the current evidence does not prove a specific native array limit. These are
numerical applicability boundaries, not claims that larger atmospheric
particles do not exist.

PyTMatrix does not provide terminal velocity. Rain uses the configured Atlas
exponential law. Frozen nodes use a bracketed Schiller-Naumann gravity/drag
solve. The empirical drag coefficient has a small jump at Re=1000; if its
one-sided force residuals straddle zero, the exact declared policy selects the
Re=1000 boundary because no exact root exists across that jump. Runtime
population evaluation replaces these normalized per-node moments with
closure-derived fall moments as required by the typed descriptor.

## Validation status

The reports exercise serialization, strict runtime binding, analytic limits,
mass accounting, native solver completion, multilinear interpolation, and the
view axis at the union of the optional 0.1-degree cut, historical 14-cut
ladder, and Build-24 VCP 12/34/35/112/212/215 centers. Long-running audit and
convergence commands snapshot generator, validator, environment, and config
hashes before computation and reject any mid-run change before atomic output.
They use the same generator, PyTMatrix implementation, dielectric formulas,
orientation model, and geometry as the LUTs, so they are
software/interpolation evidence only.

Independent Mie/PSD comparisons, covariance and propagation convention
verification, wet-particle evidence, operational calibration, and reviewed
scheme-native PSD mapping remain unmet. No report changes the embedded
`research_only_unvalidated` status.
